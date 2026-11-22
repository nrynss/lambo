/-
# Write-queue drain and durable-intent replay

Model of how the asynchronous write queue's admitted writes end: drained by
the lane workers, set aside and settled at `close()`, or replayed from their
durable intents by a later serve.

Rust items modelled (paths as at `16e9aafa`):

* `src/writeq/execution.rs`: the lane worker `WritePipeline::spawn_worker`
  (pop, the lease-lost fence, `WriteCtx::run`, which failures consume the
  intent, `settle_one`; since #295 the held writes, `Lanes::held`), and `WriteCtx::run` as far as its `ConsumeStamp`
  goes: the consume is appended under the same lock hold as the commit.
* `src/writeq/drain.rs`: `quiesce`, `abort_workers` (every unsettled receipt
  set aside), `settle_deferred` with `CloseFlush` (including the
  `close_unflushed → intent_durable` upgrade on a retried close), and
  `abort_all_sync`; and which `CloseFlush` `Memory::close` and
  `Memory::close_fenced` (`src/memory/shutdown.rs`) pass it.
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

**Two worker layers.** `workerStep`, `work` and `close` model the lane worker
**before #295**, when an in-session embedder outage (or lookup timeout)
answered `failed`; the theorems over them are marked "before #295" below and
in their doc comments, and describe behaviour the current Rust no longer has.
`stepW`/`runW` model the current worker (held writes, retries, the fence), and
`closeW` the current close; the last item below lists the theorems over them.

* `drain_fifo`, `drain_extends_taken`: a lane drained to completion has taken
  exactly the submitted jobs in submission order; what it had taken before is
  a prefix (agrees with `Receipts.lane_fifo`).
* Before #295: `drain_settles_each`, `work_log`, `drained_final`: every
  admitted write is settled once with the worker's answer (`applied` or
  `failed`), one batch per job, and no later settle changes it.
* Before #295: `workerStep_atomic`, `crash_never_reapplies`: a write in the durable log has
  its consume beside it, so no replay after a crash applies it again
  (`Graph::consume_write_intent`: "the double-apply this design excludes").
  Proved for logs shaped "all puts, then worker batches": the Rust relies on
  the admission lock order (graph write, then lanes) to put each job's put
  before its consume, and the model takes that shape as given. A crash in the
  middle of a replay is not proved (the replay's own batches have the same
  write-with-consume shape, `loop_facts`).
  Like `shutdown_no_silent_loss`, it is about one process's log over an
  empty intent table: an earlier epoch's backlog is not in the statement.
  Current counterpart: `crash_never_reappliesW`.
* Before #295: `close_settles_all`, `close_answer`, `shutdown_no_silent_loss`:
  after `close()` every receipt is settled, and each admitted write is
  drained, `intent_durable` and owed by the next replay, or `close_unflushed`
  ("deferred, not lost", `quiesce`). Current counterparts:
  `shutdown_no_silent_lossW`, `held_at_closeW`. `retry_upgrades`: the one
  settled-to-settled change (about `deferEntry`, so it holds of the current
  close too; `closeW_retry_upgradesW`). `drop_settles_nothing`: the `Drop`
  path, the documented exception; it is definitional (`abortAllSync` is the
  identity on receipts) and is there to name the gap, not to prove anything
  about it.
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
* The fix for #295 (`stepFixed` on top of `stepInput`, run through `loopD`,
  which defers an agent for the rest of the serve at a transient answer, and
  loaded by `fixedLoad`, which orders each process's rows by sequence number
  within the positions the old load gave that process):
  `loopD_no_overtake`: within a serve no write is applied while an earlier
  write of its agent is left unsettled, for any step and any run results;
  `fixed_agent_order`, `fixed_agent_order_load`: across two serves, whatever
  the embedder answers, each agent's writes of one process apply in
  admission order; `fixedLoad_sorted`, `fixed_epoch_seq_order`: per-process
  sequence order with no clock assumption; `fixedLoad_epochs`: the
  cross-process order is the old one; `fixedLoad_agrees`: while the clock
  behaves the load is unchanged; `fixed_badId_settled`: an unparseable
  receipt id is settled `failed`; `fixedD_input_goes_on`: #294 still holds.
* Before #295: `outage_failed_but_owed`: an in-session `EmbedUnavailable`
  answered `failed` and left the write owed. The current Rust does not do
  this: it holds the write (`pending_replay`), `outage_holdsW` (test
  `a_write_reached_during_an_in_session_embedder_outage_is_not_consumed`,
  which since #295 asserts `pending_replay`, then `intent_durable` at close).

* The replay after review (#295; `stepQ`, `replayP`, `serveQ`): a retry pass
  gives each deferred agent one more attempt in the serve, an id in another
  receipt format is quarantined (owed, its agent deferred) rather than
  settled, and a serve that ends with writes owed names a block reason.
  `replayP_progress`: if the retry gets an answer for every write it runs,
  nothing is owed after the serve; `replayP_preserves`,
  `hold_then_serveQ_order`: the order invariant holds through both passes,
  from the in-session hold on; `serveQ_block`: the block reason is never
  null while writes are owed; `stepQ_badId`, `stepQ_unknownFormat`,
  `stepQ_skip`: what settles and what defers. `fixed_badId_settled` describes
  the fix as first committed, which settled every unparseable id.
* The hold with admission, caps, retries and the fence (#295 review;
  `stepW`, `runW`): every event is a step (an admission, one write taken,
  one write finished, a drain-only retry, the fence). `stepW_ord`,
  `runW_ord`: per agent, settled, running, held and queued writes are its
  admitted writes in order; `stepW_caps`, `runW_caps`: the lane, queue and
  byte caps hold over queued, running and held writes at every step;
  `retry_settles_head`, `retry_enabled`: a retry needs no admission, is
  always available while writes are held, and settles the oldest held write
  once the embedder answers (that the retry comes due is the Rust's timer,
  not modelled); `stepW_caps_free`: a retry does not read the caps, by
  definition; `refused_wakes`: the commit at which a refused write woke the
  retry at once; `fence_stops_applies`: after the
  fence nothing taken is applied; `sessionW_orderInv`,
  `sessionW_then_serve_order`: per-agent order from the session through the
  next serve. `serveR_reported`: a halt that leaves intents owed reports
  `other`, never `null` (`serveQ_block` read `unset` as a reason; the stats
  print it as `null`).
* The in-session hold (#295; `drainHeld`, `arriveH`, `workH`): the lane
  worker keeps an agent's writes met by an embedder outage held, answers them
  `pending_replay`, and runs them, oldest first, before the agent's next
  write. `workH_split`, `hold_split`: for every agent, the writes settled in
  session followed by those still held are exactly its writes in admission
  order. `session_orderInv`, `hold_then_serve_order`: each agent's writes of
  one process apply in admission order in session and across the next serve,
  whatever the embedder answers. `arriveH_clears`: an agent whose held writes
  all get an answer other than an outage holds nothing after its next write.
  `OrderInv`, `loopD_preserves`, `replayD_preserves`: the order argument as an
  invariant each pass keeps.
* The close, crash and shutdown over the current worker (`closeW` after any
  `runW` trace; `InvW`, `sessionW_inv`): `shutdown_no_silent_lossW`: after a
  close every admitted write is exactly one of applied, failed with its intent
  consumed, failed after the fence with its intent left owed,
  `intent_durable` and owed in the flushed log, or `close_unflushed`, whether
  it was running, held or queued when the close began; `held_at_closeW`: a
  held write answers `pending_replay` and is owed while the session runs, and
  the close answers it `intent_durable` or `close_unflushed`, never `failed`;
  `outage_holdsW`: an outage holds the write (the current counterpart of
  `outage_failed_but_owed`); `lookupTimeout_settlesW`,
  `lookupTimeout_run_holdsW`, `heldErrW_holds`: a single lookup timeout
  settles `failed` and consumes the intent, the one ending a run of them
  holds; `closeW_retry_upgradesW`: a retried close upgrades `close_unflushed`;
  `runW_log_refines`: every batch of the current worker is one `workerStep`
  appends; `serveR_applied_pending`: the current serve applies only stored,
  unconsumed rows; `crash_never_reappliesW`: no write in a durable prefix of
  the current worker's log is applied again by the current serve.

What the model shows about the Rust (theorems in the last section):
`transient_skip_reorders` and `outage_then_replay_reorders` (a transient
embed answer breaks the per-agent order across serves; both halves fixed by
#295: the replay defers the agent, the worker holds its later writes),
`clock_step_reorders` (the load order trusts the wall clock; fixed by #295),
`outage_failed_then_applied` (before #295, a doc fix: `ReceiptAnswer::Failed`
said the write did not happen, and after an outage it later did; since #295
an outage answers `pending_replay`, not `failed`),
`store_error_blocks_backlog` (a graph-dependent refusal raised as a plain
`Invariant` stops the replay on every serve; fixed by #294, see above),
`bad_id_never_settled` (fixed by #295).

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

/-! ## The lane worker before #295 (`spawn_worker`, `execution.rs`)

**This layer describes the worker before #295, not the current Rust.**
`workerStep`, `work`, `close` and the theorems over them (through
`outage_failed_but_owed` and the examples that use them) model the worker as
it was before #295: an in-session `EmbedUnavailable` (an outage or a lookup
timeout) answered `failed`, left the intent owed, and let the agent's next
write run. The current worker holds such a write instead (`pending_replay`,
retried with backoff), settles a single lookup timeout `failed` with its
intent consumed, and holds the write that ends a run of them. It is modelled
by `stepW`/`runW` (the section "The hold with admission, caps, retries and the
fence"), and its close, crash and shutdown accounting by the section "The
close, crash and shutdown over the current worker", whose theorems are the
current counterparts named in each doc comment below. The old layer is kept,
not deleted: it is what #295 fixed (`outage_then_replay_reorders`,
`outage_failed_then_applied` are stated over it), and the lemmas about the
shared pieces (`deferEntry`, `settleDeferred`, `retry_upgrades`, the intent
table) still serve the current close. -/

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
or the lookup timeout that ends a run of them, holds the write
(`pending_replay`, nothing appended), and a single lookup timeout settles
`failed` with its intent consumed. Its batches are still `workerStep`'s
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

/-- The worker loop **before #295** over the jobs it takes, in the order it
takes them: run, settle, next (current worker: `runW`). `fence` and `out` are the lease state and the run result at that
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

/-! ## Close (`drain.rs`)

`CloseFlush`, `deferEntry`, `pendingIds` and `settleDeferred` are
`settle_deferred` and `abort_workers`' set-aside as they are today, shared by
both worker layers. `close` composes them with the worker **before #295**
(`work`); the current close is `closeW`, over `runW`. -/

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

/-- `quiesce` then the final flush then `settle_deferred`, over the worker
**before #295** (current: `closeW`). `done` is what the
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

/-! ## The worker before #295

Every theorem in this section is about `workerStep`/`work`, the worker
**before #295** (in-session outage answered `failed`). The current worker's
counterparts: `InvW` and `sessionW_inv` (what each write's receipt holds),
`runW_log_refines` (its batches are `workerStep`'s). -/

/-- The worker **before #295** only ever settles with a settled answer. The
current worker also answers `pending_replay` (unsettled) for a held write:
`InvW.held`, `held_at_closeW`. -/
theorem workerStep_settled (f : Bool) (j : Job) (r : RunResult) :
    (workerStep f j r).1.isSettled = true := by
  unfold workerStep
  cases f with
  | true => rfl
  | false =>
    cases r with
    | ok => rfl
    | err e => cases e <;> rfl

/-- The worker **before #295** answers `applied` or `failed`, nothing else.
The current worker settles only with those two as well (`InvW.settled`), but
holds an outage's write `pending_replay` instead of answering it `failed`
(`outage_holdsW`). -/
theorem workerStep_answer (f : Bool) (j : Job) (r : RunResult) :
    (workerStep f j r).1 = .applied ∨ (workerStep f j r).1 = .failed := by
  unfold workerStep
  cases f with
  | true => exact Or.inr rfl
  | false =>
    cases r with
    | ok => exact Or.inl rfl
    | err e => cases e <;> exact Or.inr rfl

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

/-- One batch per job, in the order the worker **before #295** took them.
The current worker appends none for a held or fenced write
(`runW_log_refines`, `stepW_log`). -/
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

/-- The worker **before #295** leaves the entries of other jobs alone
(current worker: the key-distinctness steps of `stepW_inv`). -/
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

/-- Each job's receipt ends with the answer of the worker **before #295** for
it, settled once (current worker: `InvW.settled`, `sessionW_inv`). -/
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
batch per job. **Before #295**: the current worker does not settle an
outage's write but holds it, so a lane does not drain to completion while the
embedder is down. Current counterparts: `runW_ord` (per agent, settled,
running, held and queued writes are its admitted writes in order) and
`shutdown_no_silent_lossW` (every admitted write's answer after a close). -/
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
settle attempt changes a drained write's receipt. Stated for the answers of
the worker **before #295**; the current worker's settled answers are the same
two (`InvW.settled`), and `settleAll_fixed` applies to them unchanged. -/
theorem drained_final (fence : Bool) (j : Job) (r : RunResult) (e : Entry)
    (h : e.answer = (workerStep fence j r).1) (later : List (Answer × Int)) :
    Lambo.Receipts.settleAll e later = e :=
  Lambo.Receipts.settleAll_fixed later e (h ▸ workerStep_settled fence j r)

/-! ## Close: drained, deferred, or answered `close_unflushed`

The `deferEntry` and `settleDeferred` lemmas here are about `settle_deferred`
as it is today and serve the current close (`closeW`) too. The theorems about
`close` (`close_settles_all`, `close_final`, `close_answer`) are over the
worker **before #295**; their current counterparts are
`shutdown_no_silent_lossW`, `held_at_closeW` and `closeW_retry_upgradesW`. -/

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
`settle_deferred` ("deferred, not lost"). Stated over `close`, the worker
**before #295** (in-session outage answered `failed`). Current counterpart:
`shutdown_no_silent_lossW` (no admitted write is left `pending` or
`pending_replay` after `closeW`). -/
theorem close_settles_all (done : List Job) (fence : Job → Bool) (out : Job → RunResult)
    (now : Int) (flush : CloseFlush) (s : RStore) :
    ∀ p ∈ (close done fence out now flush s).1, p.2.answer.isSettled = true := by
  apply settleDeferred_all_settled
  intro p hp hpi
  cases h : p.2.answer.isSettled with
  | true => rfl
  | false => exact absurd (mem_pendingIds hp h) hpi

/-- And settled for good: no later settle attempt changes any receipt after a
close (`Receipts.settleAll_fixed`). Over `close`, the worker **before #295**;
`shutdown_no_silent_lossW`'s answers for the current close are all settled, so
`settleAll_fixed` applies to them unchanged. -/
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
not. No job is in neither state. Stated over the worker **before #295**
(`workerStep`, under which an in-session outage answers `failed`), so it says
nothing about a held write. Current counterparts: `shutdown_no_silent_lossW`
(every admitted write, held and fenced writes included) and `held_at_closeW`. -/
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
outside `Entry::settle` (#117). About one entry and `deferEntry`, which is
`settle_deferred` as it is today, so it describes the current close as well;
`closeW_retry_upgradesW` states it over `closeW` and a session of the current
worker. -/
theorem retry_upgrades (now now' : Int) (e : Entry) (h : e.answer.isSettled = false) :
    (deferEntry .durable now' (deferEntry .incomplete now e)).answer = .intentRecorded := by
  have h' : ¬ e.answer.isSettled = true := by rw [h]; decide
  rw [deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now h', deferEntry_durable,
    ite_eq_left rfl]

/-- `abort_all_sync` (the `Drop` path) settles nothing: a receipt pending when
a `Memory` is dropped without `close()` stays pending in a process that is
going away. This is the documented exception to "no write is lost silently";
the write survives only if an earlier flush took its intent. `abort_all_sync`
is unchanged by #295 (it aborts the workers and settles nothing, held writes'
receipts included), so this describes the current Rust. -/
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

/-! ## The fix for #295: per-agent order, a clock-free load order, bad ids

Three changes on top of #294's step.

* An intent whose receipt id does not parse is settled `failed` (`stepFixed`),
  like a payload this build cannot decode (#129): it can never be applied or
  looked up by its id, and before it was skipped on every serve and counted
  owed forever (`bad_id_never_settled`).
* A transient embedder answer below the threshold still leaves its intent
  unconsumed, and now **defers that agent** for the rest of the serve
  (`loopD`): none of its later intents is run, so none can apply before the
  one left behind. Other agents' intents go on, and the threshold still counts
  transients across all of them. The alternative, stopping the replay at the
  first transient, also keeps the order, but makes one blip hold every
  agent's backlog and leaves `EMBEDDER_SICK_THRESHOLD` with nothing to count.
* Each process's intents are loaded in sequence order within its epoch
  (`fixedLoad`): the (`issued_ms`, `lane_seq`) load decides which epoch each
  position belongs to, as before, and the epoch's own intents fill its
  positions in `lane_seq` order (`regroup`). No clock assumption is needed.

In the Rust a deferred agent's intent is passed over after the loop's lease
and seal check; here it is passed over before `rout` is consulted, which can
only move where a halting replay stops, never what it settles. -/

/-- The replay's match after #295: an unparseable receipt id settles
`failed`; everything else is `stepInput` (#294). -/
def stepFixed (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .badId => (.failed, s)
  -- #295 as first fixed settled every unparseable id.
  | .unknownFormat => (.failed, s)
  | rr => stepInput image rr s

/-- The replay loop after #295. `d` holds the agents deferred for the rest of
the serve: an intent of one is passed over, unconsumed; a `skip` (a transient
answer below the threshold) defers the intent's agent. Otherwise as
`loopWith`. -/
def loopD (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    Streaks → List Nat → List Intent → ROut
  | _, _, [] => ROut.empty
  | s, d, i :: rest =>
    if i.job.agent ∈ d then loopD step rout s d rest
    else
      match step i.job.image (rout i.job) s with
      | (.applied, s') =>
        let o := loopD step rout s' d rest
        { o with
          answers := (i.job.key, .appliedAfterRestart) :: o.answers
          log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] :: o.log
          applied := i.job :: o.applied }
      | (.failed, s') =>
        let o := loopD step rout s' d rest
        { o with
          answers := (i.job.key, .failed) :: o.answers
          log := [.consume i.job.key .failed] :: o.log
          failed := i.job :: o.failed }
      | (.skip, s') => loopD step rout s' (i.job.agent :: d) rest
      | (.stop b, _) => { ROut.empty with block := some b }

/-- `spawn_replay` after #295: the same gate, the loop with no agent deferred
yet. -/
def replayD (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loopD step rout ⟨0, 0⟩ [] p else { ROut.empty with block := some .embedder }

/-! ### The steps -/

/-- On everything but an unparseable id the step is #294's. -/
theorem stepFixed_agrees (img : Bool) (rr : RR) (s : Streaks) (h : rr ≠ .badId)
    (h' : rr ≠ .unknownFormat) : stepFixed img rr s = stepInput img rr s := by
  unfold stepFixed
  split
  · exact absurd rfl h
  · exact absurd rfl h'
  · rfl

/-- An unparseable id settles `failed`, both streaks unchanged. -/
theorem stepFixed_badId (img : Bool) (s : Streaks) : stepFixed img .badId s = (.failed, s) := rfl

/-- #294 still holds: every input refusal settles `failed`. -/
theorem stepFixed_refusesInput (img : Bool) (e : Err) (s : Streaks) (h : e.refusesInput = true) :
    (stepFixed img (.run (.err e)) s).1 = .failed := by
  rw [stepFixed_agrees img _ s (by simp) (by simp)]
  exact stepInput_refusesInput img e s h

/-- The only result the fixed step leaves unsettled without stopping is a
transient embedder answer: so only it defers an agent. -/
theorem stepFixed_skip {img : Bool} {rr : RR} {s : Streaks} (h : (stepFixed img rr s).1 = .skip) :
    rr = .run (.err .unavailable) := by
  cases rr with
  | halt => simp [stepFixed, stepInput, replayStep] at h
  | badId => simp [stepFixed] at h
  | unknownFormat => simp [stepFixed] at h
  | decodeRefused => simp [stepFixed, stepInput, replayStep] at h
  | run r =>
    cases r with
    | ok => simp [stepFixed, stepInput, replayStep] at h
    | err e =>
      cases e <;> simp [stepFixed, stepInput, replayStep] at h ⊢
      all_goals (split at h <;> simp at h)

/-! ### The loop -/

theorem loopD_deferred {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (h : i.job.agent ∈ d) :
    loopD step rout s d (i :: rest) = loopD step rout s d rest := by
  simp [loopD, h]

theorem loopD_applied {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.applied, s')) :
    loopD step rout s d (i :: rest) =
      { loopD step rout s' d rest with
        answers := (i.job.key, .appliedAfterRestart) :: (loopD step rout s' d rest).answers
        log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] ::
          (loopD step rout s' d rest).log
        applied := i.job :: (loopD step rout s' d rest).applied } := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_failed {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.failed, s')) :
    loopD step rout s d (i :: rest) =
      { loopD step rout s' d rest with
        answers := (i.job.key, .failed) :: (loopD step rout s' d rest).answers
        log := [.consume i.job.key .failed] :: (loopD step rout s' d rest).log
        failed := i.job :: (loopD step rout s' d rest).failed } := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_skip {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.skip, s')) :
    loopD step rout s d (i :: rest) = loopD step rout s' (i.job.agent :: d) rest := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_stop {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} {b : Block}
    (hd : i.job.agent ∉ d) (h : step i.job.image (rout i.job) s = (.stop b, s')) :
    loopD step rout s d (i :: rest) = { ROut.empty with block := some b } := by
  simp only [loopD, hd, h, ite_false]

/-- `loop_facts` for the fixed loop, plus: no job of a deferred agent is
applied. -/
theorem loopD_facts (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    ∀ (l : List Intent) (s : Streaks) (d : List Nat),
    (loopD step rout s d l).applied.Sublist (l.map (·.job)) ∧
    (loopD step rout s d l).failed.Sublist (l.map (·.job)) ∧
    NoPut (loopD step rout s d l).log.flatten ∧
    (∀ x, x ∈ (loopD step rout s d l).applied ∨ x ∈ (loopD step rout s d l).failed →
      ∃ t, Mut.consume x.key t ∈ (loopD step rout s d l).log.flatten) ∧
    (∀ x ∈ (loopD step rout s d l).applied, x.agent ∉ d) := by
  intro l
  induction l with
  | nil =>
    intro s d
    refine ⟨List.Sublist.slnil, List.Sublist.slnil, ?_, ?_, ?_⟩
    · intro m hm; simp [loopD, ROut.empty] at hm
    · intro x hx; simp [loopD, ROut.empty] at hx
    · intro x hx; simp [loopD, ROut.empty] at hx
  | cons i rest ih =>
    intro s d
    by_cases hd : i.job.agent ∈ d
    · rw [loopD_deferred hd]
      obtain ⟨h1, h2, h3, h4, h5⟩ := ih s d
      exact ⟨List.Sublist.cons _ h1, List.Sublist.cons _ h2, h3, h4, h5⟩
    · cases h : step i.job.image (rout i.job) s with
      | mk st s' =>
        cases st with
        | applied =>
          rw [loopD_applied hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' d
          refine ⟨List.Sublist.cons_cons _ h1, List.Sublist.cons _ h2, ?_, ?_, ?_⟩
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
          · intro x hx
            cases List.mem_cons.mp hx with
            | inl e => rw [e]; exact hd
            | inr hx' => exact h5 x hx'
        | failed =>
          rw [loopD_failed hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' d
          refine ⟨List.Sublist.cons _ h1, List.Sublist.cons_cons _ h2, ?_, ?_, h5⟩
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
          rw [loopD_skip hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' (i.job.agent :: d)
          refine ⟨List.Sublist.cons _ h1, List.Sublist.cons _ h2, h3, h4, ?_⟩
          intro x hx hxd
          exact h5 x hx (List.mem_cons_of_mem _ hxd)
        | stop b =>
          rw [loopD_stop hd h]
          refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_, ?_⟩
          · intro m hm; simp [ROut.empty] at hm
          · intro x hx; simp [ROut.empty] at hx
          · intro x hx; simp [ROut.empty] at hx

/-- **No write overtakes an earlier one of its agent within a serve.** If the
fixed loop applies an intent, every earlier intent of the same agent in the
backlog was settled in that serve (applied or failed), never left behind for
a later serve. Holds for any step and any run results, transient answers
included: a `skip` defers the agent, so nothing of it runs after. -/
theorem loopD_no_overtake (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    ∀ (pre : List Intent) (ix : Intent) (post : List Intent) (s : Streaks) (d : List Nat),
    ((pre ++ ix :: post).map (·.job)).Nodup →
    ix.job ∈ (loopD step rout s d (pre ++ ix :: post)).applied →
    ∀ i ∈ pre, i.job.agent = ix.job.agent →
      i.job ∈ (loopD step rout s d (pre ++ ix :: post)).applied ∨
        i.job ∈ (loopD step rout s d (pre ++ ix :: post)).failed := by
  intro pre
  induction pre with
  | nil => intro ix post s d _ _ i hi; simp at hi
  | cons i0 pre' ih =>
    intro ix post s d hnd hx i hi hag
    simp only [List.cons_append] at hnd hx ⊢
    rw [List.map_cons, List.nodup_cons] at hnd
    have hne : i0.job ≠ ix.job := by
      intro e
      apply hnd.1
      rw [e]
      exact List.mem_map_of_mem (List.mem_append_right _ List.mem_cons_self)
    by_cases hd : i0.job.agent ∈ d
    · rw [loopD_deferred hd] at hx ⊢
      cases List.mem_cons.mp hi with
      | inl e =>
        subst e
        exact absurd (hag ▸ hd) ((loopD_facts step rout _ s d).2.2.2.2 _ hx)
      | inr hi' => exact ih ix post s d hnd.2 hx i hi' hag
    · cases h : step i0.job.image (rout i0.job) s with
      | mk st s' =>
        cases st with
        | applied =>
          rw [loopD_applied hd h] at hx ⊢
          have hx' := (List.mem_cons.mp hx).resolve_left (Ne.symm hne)
          cases List.mem_cons.mp hi with
          | inl e => subst e; exact Or.inl List.mem_cons_self
          | inr hi' =>
            rcases ih ix post s' d hnd.2 hx' i hi' hag with h' | h'
            · exact Or.inl (List.mem_cons_of_mem _ h')
            · exact Or.inr h'
        | failed =>
          rw [loopD_failed hd h] at hx ⊢
          cases List.mem_cons.mp hi with
          | inl e => subst e; exact Or.inr List.mem_cons_self
          | inr hi' =>
            rcases ih ix post s' d hnd.2 hx i hi' hag with h' | h'
            · exact Or.inl h'
            · exact Or.inr (List.mem_cons_of_mem _ h')
        | skip =>
          rw [loopD_skip hd h] at hx ⊢
          cases List.mem_cons.mp hi with
          | inl e =>
            subst e
            exact absurd (hag ▸ List.mem_cons_self)
              ((loopD_facts step rout _ s' _).2.2.2.2 _ hx)
          | inr hi' => exact ih ix post s' _ hnd.2 hx i hi' hag
        | stop b =>
          rw [loopD_stop hd h] at hx
          simp [ROut.empty] at hx

theorem replayD_cases (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (intents : List Intent) :
    replayD step live rout intents = ROut.empty ∨
      replayD step live rout intents = { ROut.empty with block := some .embedder } ∨
      replayD step live rout intents = loopD step rout ⟨0, 0⟩ [] (pendingOf intents) := by
  unfold replayD
  cases hp : pendingOf intents with
  | nil => exact Or.inl rfl
  | cons p ps =>
    cases live with
    | true => exact Or.inr (Or.inr rfl)
    | false => exact Or.inr (Or.inl rfl)

/-- The loop facts lifted to `replayD`, over the pending rows. -/
theorem replayD_facts (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (intents : List Intent) :
    (replayD step live rout intents).applied.Sublist ((pendingOf intents).map (·.job)) ∧
    (replayD step live rout intents).failed.Sublist ((pendingOf intents).map (·.job)) ∧
    NoPut (replayD step live rout intents).log.flatten ∧
    (∀ x, x ∈ (replayD step live rout intents).applied ∨
        x ∈ (replayD step live rout intents).failed →
      ∃ t, Mut.consume x.key t ∈ (replayD step live rout intents).log.flatten) := by
  rcases replayD_cases step live rout intents with h | h | h <;> rw [h]
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · obtain ⟨h1, h2, h3, h4, _⟩ := loopD_facts step rout _ ⟨0, 0⟩ []
    exact ⟨h1, h2, h3, h4⟩

/-- The fixed replay, too, applies only unconsumed rows. -/
theorem replayD_applied_pending {step : Bool → RR → Streaks → Step × Streaks} {live : Bool}
    {rout : Job → RR} {intents : List Intent} {x : Job}
    (hx : x ∈ (replayD step live rout intents).applied) :
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none := by
  have hs := (replayD_facts step live rout intents).1.subset hx
  obtain ⟨i, hi, rfl⟩ := List.mem_map.mp hs
  obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi
  exact ⟨i, hi1, rfl, Option.isNone_iff_eq_none.mp hi2⟩

/-- Rows with distinct receipt ids: a row is determined by its id. -/
theorem mem_eq_of_key {L : List Intent} (hnd : (L.map (·.job.key)).Nodup) {a b : Intent}
    (ha : a ∈ L) (hb : b ∈ L) (h : a.job.key = b.job.key) : a = b := by
  induction L with
  | nil => simp at ha
  | cons k L ih =>
    rw [List.map_cons, List.nodup_cons] at hnd
    cases List.mem_cons.mp ha with
    | inl ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' => rw [ha', hb']
      | inr hb' =>
        subst ha'
        exact absurd (h ▸ List.mem_map_of_mem (f := (·.job.key)) hb') hnd.1
    | inr ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' =>
        subst hb'
        exact absurd (h.symm ▸ List.mem_map_of_mem (f := (·.job.key)) ha') hnd.1
      | inr hb' => exact ih hnd.2 ha' hb'

/-- Per-epoch sequence order of a list of rows. -/
abbrev EpochSorted (l : List Intent) : Prop :=
  l.Pairwise (fun a b => a.job.epoch = b.job.epoch → a.job.rid ≤ b.job.rid)

/-- **Per-agent order across serves, whatever the embedder answers.** Two
serves of the fixed replay: the first over a backlog `L1` loaded in per-epoch
sequence order, the second over any per-epoch-sorted load `L2` of the rows
the first left. Whatever each run returned (transient embedder answers
included), the writes the two serves applied, in the order they applied
them, keep each agent's writes of one process in sequence (admission) order:
the per-agent promise (`WriteIntent::agent`, `docs/reference/mcp.mdx`) that
`transient_skip_reorders` shows the code before #295 breaking. Holds for any
step; rows are keyed by receipt id (`hnd`). -/
theorem fixed_agent_order (step : Bool → RR → Streaks → Step × Streaks) (live live' : Bool)
    (rout rout' : Job → RR) (L1 L2 : List Intent) (hnd : (L1.map (·.job.key)).Nodup)
    (hs1 : EpochSorted L1) (hs2 : EpochSorted L2)
    (hL2 : ∀ i ∈ L2, i ∈ applyLog L1 (replayD step live rout L1).log) :
    ((replayD step live rout L1).applied ++ (replayD step live' rout' L2).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  obtain ⟨f1, _, fnp, fcons⟩ := replayD_facts step live rout L1
  have hp1 : EpochSorted (pendingOf L1) := hs1.sublist List.filter_sublist
  have hsorted : ∀ (L : List Intent) (x : List Job), EpochSorted L →
      x.Sublist ((pendingOf L).map (·.job)) →
      x.Pairwise (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
    intro L x hL hx
    have hpl : ((pendingOf L).map (·.job)).Pairwise
        (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid) :=
      List.pairwise_map.mpr (hL.sublist List.filter_sublist)
    exact (hpl.sublist hx).imp
      (fun {a b} (h : a.epoch = b.epoch → a.rid ≤ b.rid) (_ : a.agent = b.agent) => h)
  refine List.pairwise_append.mpr ⟨hsorted L1 _ hs1 f1,
    hsorted L2 _ hs2 (replayD_facts step live' rout' L2).1, ?_⟩
  intro x hx y hy hag hep
  -- `y` is a row the first serve left unconsumed.
  obtain ⟨iy, hiy, hiyj, hiyo⟩ := replayD_applied_pending hy
  have hiy' := hL2 iy hiy
  -- A key the first serve consumed has no unconsumed row after it.
  have hgone : ∀ z : Job, (∃ t, Mut.consume z.key t ∈ (replayD step live rout L1).log.flatten) →
      z.key ≠ y.key := by
    intro z hz hzy
    exact foldl_consumed _ L1 fnp (Or.inl hz) iy hiy' (by rw [hiyj, hzy]) hiyo
  obtain ⟨i, hi, hij⟩ := foldl_rows _ L1 fnp hiy'
  have hio : i.outcome = none := by
    cases ho : i.outcome with
    | none => rfl
    | some t =>
      exfalso
      refine foldl_consumed _ L1 fnp (Or.inr ?_) iy hiy' rfl hiyo
      intro k hk hkk
      have hki : k = i := mem_eq_of_key hnd hk hi (by rw [hkk, hij])
      rw [hki, ho]
      exact Option.some_ne_none t
  have hip : i ∈ pendingOf L1 := List.mem_filter.mpr ⟨hi, by simp [hio]⟩
  -- `x` was applied by the loop over the pending rows.
  have hr : replayD step live rout L1 = loopD step rout ⟨0, 0⟩ [] (pendingOf L1) := by
    rcases replayD_cases step live rout L1 with h | h | h
    · rw [h] at hx; simp [ROut.empty] at hx
    · rw [h] at hx; simp [ROut.empty] at hx
    · exact h
  have hxl : x ∈ (loopD step rout ⟨0, 0⟩ [] (pendingOf L1)).applied := hr ▸ hx
  obtain ⟨ix, hixp, hixj⟩ := List.mem_map.mp (f1.subset hx)
  obtain ⟨pre, post, hsplit⟩ := List.append_of_mem hixp
  have hndp : ((pendingOf L1).map (·.job)).Nodup := by
    have hk : ((pendingOf L1).map (·.job.key)).Nodup := hnd.sublist (List.filter_sublist.map _)
    unfold List.Nodup at hk ⊢
    rw [List.pairwise_map] at hk ⊢
    exact hk.imp (fun h e => h (by rw [e]))
  rw [hsplit] at hip hp1 hxl hndp
  rw [← hixj] at hxl
  have hy' : i.job = y := by rw [← hij, hiyj]
  rcases List.mem_append.mp hip with hpre | hpost
  · -- An earlier row of the same agent: the no-overtake theorem settled it.
    exfalso
    have hsettled := loopD_no_overtake step rout pre ix post ⟨0, 0⟩ [] hndp hxl i hpre
      (by rw [hy', hixj]; exact hag.symm)
    apply hgone y _ rfl
    apply fcons
    rw [hr, hsplit, ← hy']
    exact hsettled
  · cases List.mem_cons.mp hpost with
    | inl e =>
      exfalso
      apply hgone x (fcons x (Or.inl hx))
      rw [← hy', e, hixj]
    | inr hpost' =>
      have hrel := List.rel_of_pairwise_cons (List.pairwise_append.mp hp1).2.1 hpost'
      rw [hixj, hy'] at hrel
      exact hrel hep

/-- **An unparseable receipt id is settled, not skipped forever**: the fixed
loop consumes its row `failed` and goes on (`bad_id_never_settled` is the code
before #295). -/
theorem fixed_badId_settled (rout : Job → RR) (s : Streaks) (d : List Nat) (i : Intent)
    (rest : List Intent) (he : rout i.job = .badId) (hd : i.job.agent ∉ d) :
    loopD stepFixed rout s d (i :: rest) =
      { loopD stepFixed rout s d rest with
        answers := (i.job.key, .failed) :: (loopD stepFixed rout s d rest).answers
        log := [.consume i.job.key .failed] :: (loopD stepFixed rout s d rest).log
        failed := i.job :: (loopD stepFixed rout s d rest).failed } :=
  loopD_failed hd (by rw [he]; rfl)

/-- **#294 still holds in the fixed loop**: an input refusal is consumed
`failed` and the loop goes on. -/
theorem fixedD_input_goes_on (rout : Job → RR) (s : Streaks) (d : List Nat) (i : Intent)
    (rest : List Intent) (e : Err) (he : rout i.job = .run (.err e)) (hf : e.refusesInput = true)
    (hd : i.job.agent ∉ d) :
    ∃ s', loopD stepFixed rout s d (i :: rest) =
      { loopD stepFixed rout s' d rest with
        answers := (i.job.key, .failed) :: (loopD stepFixed rout s' d rest).answers
        log := [.consume i.job.key .failed] :: (loopD stepFixed rout s' d rest).log
        failed := i.job :: (loopD stepFixed rout s' d rest).failed } := by
  have h := stepFixed_refusesInput i.job.image e s hf
  refine ⟨(stepFixed i.job.image (rout i.job) s).2, loopD_failed hd ?_⟩
  rw [he]
  exact Prod.ext h rfl

/-! ### The load order -/

/-- The intent with the smallest sequence number among `x` and those of `l`
in `x`'s epoch (`x` itself, or the first such, on a tie). -/
def minE (x : Intent) : List Intent → Intent
  | [] => x
  | y :: ys =>
    minE (if y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid then y else x) ys

/-- `l` with its first `m` replaced by `x`. -/
def swapIn (m x : Intent) : List Intent → List Intent
  | [] => []
  | y :: ys => if y = m then x :: ys else y :: swapIn m x ys

/-- Selection within epochs: each position keeps its epoch, and an epoch's
positions take its intents in sequence order. `n` is fuel (`regroup` passes
the length). -/
def regroupN : Nat → List Intent → List Intent
  | 0, l => l
  | _, [] => []
  | n + 1, x :: xs =>
    if minE x xs = x then x :: regroupN n xs
    else minE x xs :: regroupN n (swapIn (minE x xs) x xs)

def regroup (l : List Intent) : List Intent := regroupN l.length l

/-- **The load order after #295** (`replay_order` in `replay.rs`): the
adapters' (`issued_ms`, `lane_seq`) order picks which process each position
belongs to, and each process's intents fill its positions in `lane_seq`
order. One difference from the Rust: every row here has an epoch, so a row
whose id the replay cannot parse (`badId`, `unknownFormat`, outcomes of the
run, not of the row) is regrouped with its process; `replay_order` cannot
read an epoch from such an id and leaves the row where the load put it. Such
a row is never applied, so the order theorems are unaffected. -/
def fixedLoad (l : List Intent) : List Intent := regroup (loadOrder l)

theorem minE_spec : ∀ (l : List Intent) (x : Intent),
    (minE x l).job.epoch = x.job.epoch ∧ minE x l ∈ x :: l ∧
      ∀ y ∈ x :: l, y.job.epoch = x.job.epoch → (minE x l).job.rid ≤ y.job.rid := by
  intro l
  induction l with
  | nil =>
    intro x
    refine ⟨rfl, List.mem_cons_self, ?_⟩
    intro y hy _
    rw [List.mem_singleton.mp hy]
    exact Nat.le_refl _
  | cons y ys ih =>
    intro x
    simp only [minE]
    by_cases hc : y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid
    · rw [ite_eq_left hc]
      obtain ⟨h1, h2, h3⟩ := ih y
      refine ⟨h1.trans hc.1, ?_, ?_⟩
      · exact List.mem_cons_of_mem _ h2
      · intro z hz hze
        cases List.mem_cons.mp hz with
        | inl e => rw [e]; exact Nat.le_of_lt (Nat.lt_of_le_of_lt (h3 y List.mem_cons_self rfl) hc.2)
        | inr hz' => exact h3 z hz' (hze.trans hc.1.symm)
    · rw [ite_eq_right hc]
      obtain ⟨h1, h2, h3⟩ := ih x
      refine ⟨h1, ?_, ?_⟩
      · cases List.mem_cons.mp h2 with
        | inl e => rw [e]; exact List.mem_cons_self
        | inr h' => exact List.mem_cons_of_mem _ (List.mem_cons_of_mem _ h')
      · intro z hz hze
        cases List.mem_cons.mp hz with
        | inl e => rw [e]; exact h3 x List.mem_cons_self rfl
        | inr hz' =>
          cases List.mem_cons.mp hz' with
          | inl e =>
            rw [e] at hze ⊢
            have hxy : x.job.rid ≤ y.job.rid := by
              rcases Nat.lt_or_ge y.job.rid x.job.rid with hl | hl
              · exact absurd ⟨hze, hl⟩ hc
              · exact hl
            exact Nat.le_trans (h3 x List.mem_cons_self rfl) hxy
          | inr hz'' => exact h3 z (List.mem_cons_of_mem _ hz'') hze

/-- With nothing earlier in sequence in its epoch, `x` is its own minimum. -/
theorem minE_self : ∀ (l : List Intent) (x : Intent),
    (∀ y ∈ l, y.job.epoch = x.job.epoch → x.job.rid ≤ y.job.rid) → minE x l = x := by
  intro l
  induction l with
  | nil => intro x _; rfl
  | cons y ys ih =>
    intro x h
    simp only [minE]
    have hc : ¬ (y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid) := by
      intro hc
      exact absurd (h y List.mem_cons_self hc.1) (Nat.not_le_of_lt hc.2)
    rw [ite_eq_right hc]
    exact ih x (fun z hz => h z (List.mem_cons_of_mem _ hz))

theorem swapIn_perm (m x : Intent) : ∀ (xs : List Intent), m ∈ xs →
    (x :: xs).Perm (m :: swapIn m x xs) := by
  intro xs
  induction xs with
  | nil => intro h; simp at h
  | cons y ys ih =>
    intro h
    simp only [swapIn]
    by_cases hy : y = m
    · rw [ite_eq_left hy, hy]
      exact List.Perm.swap m x ys
    · rw [ite_eq_right hy]
      have hm : m ∈ ys := (List.mem_cons.mp h).resolve_left (Ne.symm hy)
      exact ((List.Perm.swap y x ys).trans ((ih hm).cons y)).trans (List.Perm.swap m y _)

theorem swapIn_length (m x : Intent) : ∀ (xs : List Intent), (swapIn m x xs).length = xs.length := by
  intro xs
  induction xs with
  | nil => rfl
  | cons y ys ih =>
    simp only [swapIn]
    split <;> simp [ih]

theorem swapIn_epochs (m x : Intent) (h : m.job.epoch = x.job.epoch) : ∀ (xs : List Intent),
    (swapIn m x xs).map (·.job.epoch) = xs.map (·.job.epoch) := by
  intro xs
  induction xs with
  | nil => rfl
  | cons y ys ih =>
    simp only [swapIn]
    by_cases hy : y = m
    · rw [ite_eq_left hy, hy]; simp [h]
    · rw [ite_eq_right hy]; simp [ih]

theorem regroupN_perm : ∀ (n : Nat) (l : List Intent), (regroupN n l).Perm l := by
  intro n
  induction n with
  | zero => intro l; exact List.Perm.refl _
  | succ n ih =>
    intro l
    cases l with
    | nil => exact List.Perm.refl _
    | cons x xs =>
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]; exact (ih xs).cons x
      · rw [ite_eq_right hm]
        have hmem : minE x xs ∈ xs :=
          (List.mem_cons.mp (minE_spec xs x).2.1).resolve_left hm
        exact ((ih _).cons _).trans (swapIn_perm _ x xs hmem).symm

theorem regroupN_epochs : ∀ (n : Nat) (l : List Intent),
    (regroupN n l).map (·.job.epoch) = l.map (·.job.epoch) := by
  intro n
  induction n with
  | zero => intro l; rfl
  | succ n ih =>
    intro l
    cases l with
    | nil => rfl
    | cons x xs =>
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]; simp [ih]
      · rw [ite_eq_right hm]
        have he := (minE_spec xs x).1
        simp only [List.map_cons, ih, swapIn_epochs _ x he, he]

theorem regroupN_sorted : ∀ (n : Nat) (l : List Intent), l.length ≤ n →
    EpochSorted (regroupN n l) := by
  intro n
  induction n with
  | zero =>
    intro l hl
    rw [List.length_eq_zero_iff.mp (Nat.le_zero.mp hl)]
    exact List.Pairwise.nil
  | succ n ih =>
    intro l hl
    cases l with
    | nil => exact List.Pairwise.nil
    | cons x xs =>
      have hxs : xs.length ≤ n := Nat.le_of_succ_le_succ hl
      obtain ⟨he, hmem, hmin⟩ := minE_spec xs x
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]
        refine List.pairwise_cons.mpr ⟨?_, ih xs hxs⟩
        intro z hz hze
        have hz' := (regroupN_perm n xs).mem_iff.mp hz
        rw [← hm]
        exact hmin z (List.mem_cons_of_mem _ hz') (hm ▸ hze.symm)
      · rw [ite_eq_right hm]
        have hin : minE x xs ∈ xs := (List.mem_cons.mp hmem).resolve_left hm
        refine List.pairwise_cons.mpr ⟨?_, ih _ (by rw [swapIn_length]; exact hxs)⟩
        intro z hz hze
        have hz' := (regroupN_perm n _).mem_iff.mp hz
        have hz'' : z ∈ x :: xs :=
          (swapIn_perm _ x xs hin).mem_iff.mpr (List.mem_cons_of_mem _ hz')
        exact hmin z hz'' (hze.symm.trans he)

theorem regroupN_of_sorted : ∀ (n : Nat) (l : List Intent), EpochSorted l → regroupN n l = l := by
  intro n
  induction n with
  | zero => intro l _; rfl
  | succ n ih =>
    intro l hl
    cases l with
    | nil => rfl
    | cons x xs =>
      have hl' := List.pairwise_cons.mp hl
      have hm : minE x xs = x :=
        minE_self xs x (fun y hy hye => hl'.1 y hy hye.symm)
      simp only [regroupN, hm, ite_true]
      rw [ih xs hl'.2]

theorem insertK_perm (x : Intent) : ∀ (l : List Intent), (insertK x l).Perm (x :: l) := by
  intro l
  induction l with
  | nil => exact List.Perm.refl _
  | cons y ys ih =>
    unfold insertK
    split
    · exact List.Perm.refl _
    · exact (ih.cons y).trans (List.Perm.swap x y ys)

theorem loadOrder_perm : ∀ (l : List Intent), (loadOrder l).Perm l := by
  intro l
  induction l with
  | nil => exact List.Perm.refl _
  | cons x xs ih => exact (insertK_perm x _).trans (ih.cons x)

/-- The fixed load holds the same rows. -/
theorem fixedLoad_perm (l : List Intent) : (fixedLoad l).Perm l :=
  (regroupN_perm _ _).trans (loadOrder_perm l)

/-- **Each process's intents load in sequence order, with no clock
assumption** (`load_epoch_seq_order` needs one; `clock_step_reorders` is
what happens without it). -/
theorem fixedLoad_sorted (l : List Intent) : EpochSorted (fixedLoad l) :=
  regroupN_sorted _ _ (Nat.le_refl _)

/-- **Cross-process order as before**: each position of the load belongs to
the same process as in the (`issued_ms`, `lane_seq`) order. -/
theorem fixedLoad_epochs (l : List Intent) :
    (fixedLoad l).map (·.job.epoch) = (loadOrder l).map (·.job.epoch) :=
  regroupN_epochs _ _

/-- **Nothing changes while the clock behaves**: under `load_epoch_seq_order`'s
hypothesis the fixed load is the old one. -/
theorem fixedLoad_agrees (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) :
    fixedLoad l = loadOrder l :=
  regroupN_of_sorted _ _ (load_epoch_seq_order l hmono)

/-- One serve of the fixed replay over the fixed load applies each process's
writes in sequence order: `replay_epoch_seq_order` without its clock
hypothesis. -/
theorem fixed_epoch_seq_order (live : Bool) (rout : Job → RR) (l : List Intent) :
    (replayD stepFixed live rout (fixedLoad l)).applied.Pairwise
      (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hs := (replayD_facts stepFixed live rout (fixedLoad l)).1
  apply List.Pairwise.sublist hs
  exact List.pairwise_map.mpr ((fixedLoad_sorted l).sublist List.filter_sublist)

/-- **The two fixes together, across two serves**: rows keyed by receipt id,
loaded by the fixed load at each serve; whatever the embedder answers, the
writes the two serves apply keep each agent's writes of one process in
admission order. -/
theorem fixed_agent_order_load (live live' : Bool) (rout rout' : Job → RR) (rows : List Intent)
    (hnd : (rows.map (·.job.key)).Nodup) :
    let L1 := fixedLoad rows
    let r1 := replayD stepFixed live rout L1
    ((r1.applied ++ (replayD stepFixed live' rout' (fixedLoad (applyLog L1 r1.log))).applied)).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  intro L1 r1
  have hnd1 : (L1.map (·.job.key)).Nodup := ((fixedLoad_perm rows).map _).nodup_iff.mpr hnd
  exact fixed_agent_order stepFixed live live' rout rout' L1 _ hnd1 (fixedLoad_sorted rows)
    (fixedLoad_sorted _) (fun i hi => (fixedLoad_perm _).mem_iff.mp hi)

/-! ## Order across passes: the invariant the order proofs carry

`fixed_agent_order` compares two serves. The in-session hold and the replay's
retry pass add more passes, so the argument is stated once as an invariant
over the rows left owed and the writes applied so far, and each pass is shown
to keep it. -/

/-- The writes applied so far keep each agent's writes of one process in
sequence order, and none is later in sequence than a write of its agent and
process still owed in `rows`. -/
def OrderInv (rows : List Intent) (applied : List Job) : Prop :=
  applied.Pairwise (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) ∧
  ∀ x ∈ applied, ∀ i ∈ rows, i.outcome = none → x.agent = i.job.agent →
    x.epoch = i.job.epoch → x.rid ≤ i.job.rid

theorem orderInv_nil (rows : List Intent) : OrderInv rows [] :=
  ⟨List.Pairwise.nil, fun _ hx => by simp at hx⟩

/-- Reloading the rows in another order keeps the invariant. -/
theorem orderInv_perm {rows rows' : List Intent} {A : List Job} (hp : rows'.Perm rows)
    (h : OrderInv rows A) : OrderInv rows' A :=
  ⟨h.1, fun x hx i hi => h.2 x hx i (hp.mem_iff.mp hi)⟩

theorem applyMut_jobs (acc : List Intent) (m : Mut) (hm : ∀ j, m ≠ .put j) :
    (applyMut acc m).map (·.job) = acc.map (·.job) := by
  cases m with
  | put k => exact absurd rfl (hm k)
  | write _ => rfl
  | consume r t =>
    simp only [applyMut, List.map_map]
    apply List.map_congr_left
    intro i _
    by_cases h : i.job.key = r <;> simp [h]

/-- Consumes keep the rows and their order. -/
theorem foldl_jobs : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    (ms.foldl applyMut acc).map (·.job) = acc.map (·.job) := by
  intro ms
  induction ms with
  | nil => intro acc _; rfl
  | cons m ms ih =>
    intro acc hnp
    simp only [List.foldl_cons]
    rw [ih _ (fun x hx => hnp x (List.mem_cons_of_mem _ hx)),
      applyMut_jobs acc m (hnp m List.mem_cons_self)]

/-- **One pass of the fixed loop keeps the invariant**, for any step, any
streaks and any run results, over rows keyed by receipt id and loaded in
per-process sequence order. -/
theorem loopD_preserves (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR)
    (s : Streaks) (rows : List Intent) (A : List Job) (hnd : (rows.map (·.job.key)).Nodup)
    (hs : EpochSorted rows) (hI : OrderInv rows A) :
    OrderInv (applyLog rows (loopD step rout s [] (pendingOf rows)).log)
      (A ++ (loopD step rout s [] (pendingOf rows)).applied) := by
  obtain ⟨f1, _, fnp, fcons, _⟩ := loopD_facts step rout (pendingOf rows) s []
  have hp1 : EpochSorted (pendingOf rows) := hs.sublist List.filter_sublist
  have hpa : (loopD step rout s [] (pendingOf rows)).applied.Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
    have hpl : ((pendingOf rows).map (·.job)).Pairwise
        (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid) := List.pairwise_map.mpr hp1
    exact (hpl.sublist f1).imp
      (fun {a b} (h : a.epoch = b.epoch → a.rid ≤ b.rid) (_ : a.agent = b.agent) => h)
  refine ⟨List.pairwise_append.mpr ⟨hI.1, hpa, ?_⟩, ?_⟩
  · intro x hx y hy hag hep
    obtain ⟨iy, hiy, rfl⟩ := List.mem_map.mp (f1.subset hy)
    obtain ⟨hiy1, hiy2⟩ := List.mem_filter.mp hiy
    exact hI.2 x hx iy hiy1 (Option.isNone_iff_eq_none.mp hiy2) hag hep
  · intro x hx i' hi' hio' hag hep
    -- The row `i'` left owed comes from a row `i` owed before the pass.
    have hgone : ∀ z : Job,
        (∃ t, Mut.consume z.key t ∈ (loopD step rout s [] (pendingOf rows)).log.flatten) →
        z.key ≠ i'.job.key := by
      intro z hz hzk
      exact foldl_consumed _ rows fnp (Or.inl hz) i' hi' hzk.symm hio'
    obtain ⟨i, hi, hij⟩ := foldl_rows _ rows fnp hi'
    have hio : i.outcome = none := by
      cases ho : i.outcome with
      | none => rfl
      | some t =>
        exfalso
        refine foldl_consumed _ rows fnp (Or.inr ?_) i' hi' rfl hio'
        intro k hk hkk
        have hki : k = i := mem_eq_of_key hnd hk hi (by rw [hkk, hij])
        rw [hki, ho]
        exact Option.some_ne_none t
    rw [hij] at hag hep ⊢
    cases List.mem_append.mp hx with
    | inl hxA => exact hI.2 x hxA i hi hio hag hep
    | inr hxl =>
      have hip : i ∈ pendingOf rows := List.mem_filter.mpr ⟨hi, by simp [hio]⟩
      obtain ⟨ix, hixp, hixj⟩ := List.mem_map.mp (f1.subset hxl)
      obtain ⟨pre, post, hsplit⟩ := List.append_of_mem hixp
      have hndp : ((pendingOf rows).map (·.job)).Nodup := by
        have hk : ((pendingOf rows).map (·.job.key)).Nodup :=
          hnd.sublist (List.filter_sublist.map _)
        unfold List.Nodup at hk ⊢
        rw [List.pairwise_map] at hk ⊢
        exact hk.imp (fun h e => h (by rw [e]))
      have hxl' := hxl
      rw [hsplit] at hip hp1 hxl' hndp
      rw [← hixj] at hxl'
      rcases List.mem_append.mp hip with hpre | hpost
      · exfalso
        have hsettled := loopD_no_overtake step rout pre ix post s [] hndp hxl' i hpre
          (by rw [hixj]; exact hag.symm)
        apply hgone i.job _ (by rw [hij])
        apply fcons
        rw [hsplit]
        exact hsettled
      · cases List.mem_cons.mp hpost with
        | inl e =>
          exfalso
          apply hgone x (fcons x (Or.inl hxl))
          rw [hij, e, hixj]
        | inr hpost' =>
          have hrel := List.rel_of_pairwise_cons (List.pairwise_append.mp hp1).2.1 hpost'
          rw [hixj] at hrel
          exact hrel hep

/-- The same for a whole serve of the fixed replay (the liveness gate
included). -/
theorem replayD_preserves (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (rows : List Intent) (A : List Job) (hnd : (rows.map (·.job.key)).Nodup)
    (hs : EpochSorted rows) (hI : OrderInv rows A) :
    OrderInv (applyLog rows (replayD step live rout rows).log)
      (A ++ (replayD step live rout rows).applied) := by
  rcases replayD_cases step live rout rows with h | h | h <;> rw [h]
  · simpa [ROut.empty, applyLog] using hI
  · simpa [ROut.empty, applyLog] using hI
  · exact loopD_preserves step rout ⟨0, 0⟩ rows A hnd hs hI

/-! ## The in-session hold (#295)

Before, a write the lane worker met during an embedder outage answered
`failed`, its intent was left owed, and the worker went on to the agent's next
write, which applied ahead of it; the next serve then applied the first
(`outage_then_replay_reorders`). Now the worker keeps the agent's owed writes
(`held`): at each later write of the agent it first runs the held ones,
oldest first, then the new one, and stops at the first that meets an outage.
That one and every later write of the agent stay owed: their intents
unconsumed, their receipts `pending_replay` (unsettled, so a close sets them
aside and answers `intent_durable`, and the next serve replays them in
order). A held write that is later applied in this serve answers `applied`,
a refused one `failed`; so the receipt of a write met by an outage no longer
says `failed` first and something else later.

`out t` is what a run at the `t`-th arrival returns, so a retry can answer
differently from the first attempt. The lease fence is not modelled here: it
refuses the arriving write without running anything, as before, and runs no
held one. -/

/-- Run an agent's owed jobs, oldest first, until one meets an embedder
outage: what was settled (job and answer, in order), the batches appended,
and the jobs still owed (that one and every later one). -/
def drainHeld (o : Job → RunResult) : List Job → List (Job × Answer) × List Batch × List Job
  | [] => ([], [], [])
  | j :: js =>
    match o j with
    | .ok =>
      let r := drainHeld o js
      ((j, .applied) :: r.1, [.write j.key, .consume j.key .applied] :: r.2.1, r.2.2)
    | .err e =>
      if e.transient then ([], [], j :: js)
      else
        let r := drainHeld o js
        ((j, .failed) :: r.1, [.consume j.key .failed] :: r.2.1, r.2.2)

/-- The in-session workers' state: receipts, the held jobs (every agent's, in
arrival order), the batches appended, and what was settled, in order. -/
structure HState where
  store : RStore
  held : List Job
  log : List Batch
  settled : List (Job × Answer)

def settleList (now : Int) (s : RStore) (xs : List (Job × Answer)) : RStore :=
  xs.foldl (fun s p => settleOne s p.1.key p.2 now) s

def holdList (now : Int) (s : RStore) (js : List Job) : RStore :=
  js.foldl (fun s j => settleOne s j.key .pendingReplay now) s

/-- One write `j` taken off its lane: the agent's held writes, then `j`. -/
def arriveH (o : Job → RunResult) (now : Int) (st : HState) (j : Job) : HState :=
  let r := drainHeld o (st.held.filter (fun k => k.agent = j.agent) ++ [j])
  { store := holdList now (settleList now st.store r.1) r.2.2
    held := st.held.filter (fun k => k.agent ≠ j.agent) ++ r.2.2
    log := st.log ++ r.2.1
    settled := st.settled ++ r.1 }

/-- The workers over the writes they take, in the order they take them. -/
def workH (out : Nat → Job → RunResult) (now : Int) : Nat → HState → List Job → HState
  | _, st, [] => st
  | t, st, j :: js => workH out now (t + 1) (arriveH (out t) now st j) js

/-- The writes applied in session, in the order they were applied. -/
def HState.applied (st : HState) : List Job :=
  (st.settled.filter (fun p => p.2 = .applied)).map (·.1)

theorem drainHeld_split (o : Job → RunResult) : ∀ (l : List Job),
    (drainHeld o l).1.map (·.1) ++ (drainHeld o l).2.2 = l := by
  intro l
  induction l with
  | nil => rfl
  | cons j js ih =>
    unfold drainHeld
    cases o j with
    | ok => simp [ih]
    | err e =>
      by_cases h : e.transient = true
      · simp [h]
      · simp [h, ih]

/-- With the embedder answering every held write, the agent's held writes all
settle. -/
theorem drainHeld_clears (o : Job → RunResult) : ∀ (l : List Job),
    (∀ k ∈ l, ∀ e, o k = .err e → e.transient = false) → (drainHeld o l).2.2 = [] := by
  intro l
  induction l with
  | nil => intro _; rfl
  | cons j js ih =>
    intro h
    have ht := h j List.mem_cons_self
    have hr := ih (fun k hk => h k (List.mem_cons_of_mem _ hk))
    unfold drainHeld
    cases hj : o j with
    | ok => simpa using hr
    | err e =>
      have := ht e hj
      simp [this, hr]

theorem filter_agent_of_all {l : List Job} {b : Nat} (hl : ∀ k ∈ l, k.agent = b) (a : Nat) :
    l.filter (fun k => k.agent = a) = if b = a then l else [] := by
  by_cases hab : b = a
  · simp only [hab, ite_true]
    exact List.filter_eq_self.mpr (fun k hk => by simp [hl k hk, hab])
  · simp only [hab, ite_false]
    exact List.filter_eq_nil_iff.mpr (fun k hk => by simp [hl k hk, hab])

/-- One arrival moves only its agent's writes: those settled and those held
are, together, that agent's held writes before it and the arriving one, in
order; every other agent is untouched. -/
theorem arriveH_split (o : Job → RunResult) (now : Int) (st : HState) (j : Job) (a : Nat) :
    ((arriveH o now st j).settled.map (·.1)).filter (fun k => k.agent = a) ++
        (arriveH o now st j).held.filter (fun k => k.agent = a) =
      (st.settled.map (·.1)).filter (fun k => k.agent = a) ++
        st.held.filter (fun k => k.agent = a) ++ [j].filter (fun k => k.agent = a) := by
  have hsplit := drainHeld_split o (st.held.filter (fun k => k.agent = j.agent) ++ [j])
  have hall : ∀ k ∈ st.held.filter (fun k => k.agent = j.agent) ++ [j], k.agent = j.agent := by
    intro k hk
    rcases List.mem_append.mp hk with hk | hk
    · simpa using (List.mem_filter.mp hk).2
    · rw [List.mem_singleton.mp hk]
  have hM := filter_agent_of_all hall a
  rw [← hsplit, List.filter_append] at hM
  simp only [arriveH, List.map_append, List.filter_append, List.append_assoc]
  by_cases hab : j.agent = a
  · subst hab
    simp only [ite_true] at hM
    have ho : (st.held.filter (fun k => k.agent ≠ j.agent)).filter (fun k => k.agent = j.agent)
        = [] := by
      rw [List.filter_filter]
      exact List.filter_eq_nil_iff.mpr (fun k _ => by by_cases h : k.agent = j.agent <;> simp [h])
    rw [ho, List.nil_append, hM, hsplit]
    simp
  · simp only [hab, ite_false] at hM
    have h1 := (List.append_eq_nil_iff.mp hM).1
    have h2 := (List.append_eq_nil_iff.mp hM).2
    rw [h1, h2, List.filter_filter]
    simp only [hab, List.nil_append, List.append_nil, List.filter_cons, List.filter_nil,
      decide_false, Bool.false_eq_true, ite_false]
    congr 1
    apply List.filter_congr
    intro k _
    by_cases h : k.agent = a
    · have : ¬ a = j.agent := fun e => hab e.symm
      simp [h, this]
    · simp [h]

/-- **The hold, over a whole serve.** For every agent, the writes it settled
in session (applied or failed) followed by the writes it still has held are
exactly its writes, in the order its lane delivered them (admission order): a
held write is never followed by a settled one, so no write of the agent is
applied while an earlier one is owed. -/
theorem workH_split (out : Nat → Job → RunResult) (now : Int) (a : Nat) :
    ∀ (js : List Job) (t : Nat) (st : HState),
    ((workH out now t st js).settled.map (·.1)).filter (fun k => k.agent = a) ++
        (workH out now t st js).held.filter (fun k => k.agent = a) =
      (st.settled.map (·.1)).filter (fun k => k.agent = a) ++
        st.held.filter (fun k => k.agent = a) ++ js.filter (fun k => k.agent = a) := by
  intro js
  induction js with
  | nil => intro t st; simp [workH]
  | cons j js ih =>
    intro t st
    simp only [workH]
    rw [ih, arriveH_split]
    rw [show j :: js = [j] ++ js from rfl, List.filter_append]
    simp only [List.append_assoc]

/-- A serve that starts with nothing held. -/
def HState.init (s : RStore) : HState := ⟨s, [], [], []⟩

theorem hold_split (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job)
    (a : Nat) :
    ((workH out now 0 (HState.init s) js).settled.map (·.1)).filter (fun k => k.agent = a) ++
        (workH out now 0 (HState.init s) js).held.filter (fun k => k.agent = a) =
      js.filter (fun k => k.agent = a) := by
  rw [workH_split]
  simp [HState.init]

/-- A list whose every agent's sub-list is ordered is ordered for each agent. -/
theorem pairwise_of_agent_filters {P : Job → Job → Prop} :
    ∀ (l : List Job), (∀ a, (l.filter (fun k => k.agent = a)).Pairwise P) →
      l.Pairwise (fun x y => x.agent = y.agent → P x y) := by
  intro l
  induction l with
  | nil => intro _; exact List.Pairwise.nil
  | cons x xs ih =>
    intro h
    refine List.pairwise_cons.mpr ⟨?_, ih ?_⟩
    · intro y hy hxy
      have hx := h x.agent
      simp only [List.filter_cons, decide_true, ite_true] at hx
      exact (List.pairwise_cons.mp hx).1 y (List.mem_filter.mpr ⟨hy, by simp [hxy]⟩)
    · intro a
      have hx := h a
      by_cases hxa : x.agent = a
      · simp only [List.filter_cons, hxa, decide_true, ite_true] at hx
        exact (List.pairwise_cons.mp hx).2
      · simpa [List.filter_cons, hxa] using hx

/-- **In session, each agent's writes apply in admission order, and none
before an earlier write of its agent still owed.** Writes of one process
(epoch `e`), its lanes delivering each agent's writes in sequence order. The
writes applied in session satisfy the order invariant against any rows whose
owed writes of this process are the ones still held: so the next serve,
whatever it does, keeps the order (`hold_then_serve_order`). -/
theorem session_orderInv (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job)
    (e : Nat) (he : ∀ j ∈ js, j.epoch = e)
    (hjs : js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid)) (rows : List Intent)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held) :
    OrderInv rows (workH out now 0 (HState.init s) js).applied := by
  let st := workH out now 0 (HState.init s) js
  have hsplit := hold_split out now s js
  have hsub : st.applied.Sublist (st.settled.map (·.1)) :=
    (List.filter_sublist).map _
  -- Each agent's writes, in lane order, are in sequence order.
  have hja : ∀ a, (js.filter (fun k => k.agent = a)).Pairwise (fun x y => x.rid < y.rid) := by
    intro a
    apply List.Pairwise.imp_of_mem _ (hjs.sublist List.filter_sublist)
    intro x y hx hy hxy
    exact hxy (by rw [(by simpa using (List.mem_filter.mp hx).2 : x.agent = a),
      (by simpa using (List.mem_filter.mp hy).2 : y.agent = a)])
  have hsettled_mem : ∀ x ∈ st.settled.map (·.1), x ∈ js := by
    intro x hx
    have : x ∈ (st.settled.map (·.1)).filter (fun k => k.agent = x.agent) :=
      List.mem_filter.mpr ⟨hx, by simp⟩
    have h2 := hsplit x.agent
    have : x ∈ js.filter (fun k => k.agent = x.agent) := by
      rw [← h2]; exact List.mem_append_left _ this
    exact (List.mem_filter.mp this).1
  refine ⟨?_, ?_⟩
  · apply List.Pairwise.sublist hsub
    apply pairwise_of_agent_filters
    intro a
    have hpre : ((st.settled.map (·.1)).filter (fun k => k.agent = a)).Sublist
        (js.filter (fun k => k.agent = a)) := by
      rw [← hsplit a]; exact List.sublist_append_left _ _
    exact ((hja a).sublist hpre).imp
      (fun {x y} (h : x.rid < y.rid) (_ : x.epoch = y.epoch) => Nat.le_of_lt h)
  · intro x hx i hi hio hag hep
    have hxs := hsub.subset hx
    have hxe : x.epoch = e := he x (hsettled_mem x hxs)
    have hih := hrows i hi hio (hep ▸ hxe)
    have hpa := (hja x.agent)
    rw [← hsplit x.agent] at hpa
    have hcross := (List.pairwise_append.mp hpa).2.2
    exact Nat.le_of_lt (hcross x (List.mem_filter.mpr ⟨hxs, by simp⟩) i.job
      (List.mem_filter.mpr ⟨hih, by simp [hag]⟩))

/-- **Per-agent order in session and across the next serve.** The writes
applied in session, then those the next serve's fixed replay applies from the
rows left (every owed row of this process being one still held), keep each
agent's writes of one process in admission order, whatever the embedder
answered at any point. The fix for `outage_then_replay_reorders`. -/
theorem hold_then_serve_order (out : Nat → Job → RunResult) (now : Int) (s : RStore)
    (js : List Job) (e : Nat) (he : ∀ j ∈ js, j.epoch = e)
    (hjs : js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid)) (rows : List Intent)
    (hnd : (rows.map (·.job.key)).Nodup)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held)
    (live : Bool) (rout : Job → RR) :
    ((workH out now 0 (HState.init s) js).applied ++
        (replayD stepFixed live rout (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hI := orderInv_perm (fixedLoad_perm rows) (session_orderInv out now s js e he hjs rows hrows)
  have hnd' : ((fixedLoad rows).map (·.job.key)).Nodup :=
    ((fixedLoad_perm rows).map _).nodup_iff.mpr hnd
  exact (replayD_preserves stepFixed live rout _ _ hnd' (fixedLoad_sorted rows) hI).1

/-- **Progress in session**: at a write of an agent whose held writes and the
new one all get an answer other than an outage, nothing of that agent stays
held. -/
theorem arriveH_clears (o : Job → RunResult) (now : Int) (st : HState) (j : Job)
    (h : ∀ k ∈ st.held.filter (fun k => k.agent = j.agent) ++ [j], ∀ e, o k = .err e →
      e.transient = false) :
    (arriveH o now st j).held.filter (fun k => k.agent = j.agent) = [] := by
  simp only [arriveH, drainHeld_clears o _ h, List.append_nil, List.filter_filter]
  exact List.filter_eq_nil_iff.mpr (fun k _ => by by_cases hk : k.agent = j.agent <;> simp [hk])

/-! ## The replay's retry pass, quarantine and block reason (#295)

Three changes to the fixed replay, after review.

* **Retry pass** (`replayP`). An agent deferred at a transient answer used to
  wait for the next serve, so a write that answers transiently every time held
  its agent's backlog forever. Now, unless the pass stopped, the replay runs
  once more over what it left owed (the deferred agents' writes, from each
  one's oldest owed write), with the streaks it ended with: each deferred
  agent gets one more attempt in the serve. `replayP_progress` says what that
  buys; a write that never embeds still defers its agent every serve (an
  owner question).
* **Quarantine** (`stepQ`). An id in another receipt format (`unknownFormat`,
  as a newer build writes) is not settled `failed`, which cannot be undone:
  it is left owed and its agent deferred, like a transient answer, so its
  agent's later writes do not overtake it. A malformed id in this build's own
  format (`badId`) is still settled `failed`.
* **Block reason** (`serveQ`). A serve that ends with writes owed and no stop
  now says so: `embedder` if the last pass deferred an agent at a transient
  answer, `other` if only quarantined ids hold writes back. -/

/-- The replay's step after the remediation: `unknownFormat` defers its
agent (a `skip`); everything else is `stepFixed`. -/
def stepQ (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .unknownFormat => (.skip, s)
  | rr => stepFixed image rr s

theorem stepQ_agrees (img : Bool) (rr : RR) (s : Streaks) (h : rr ≠ .unknownFormat) :
    stepQ img rr s = stepFixed img rr s := by
  unfold stepQ
  split
  · exact absurd rfl h
  · rfl

/-- A malformed id in this build's format is still settled `failed`. -/
theorem stepQ_badId (img : Bool) (s : Streaks) : stepQ img .badId s = (.failed, s) := rfl

/-- An id in another format is quarantined: left owed, its agent deferred,
the streaks untouched. -/
theorem stepQ_unknownFormat (img : Bool) (s : Streaks) :
    stepQ img .unknownFormat s = (.skip, s) := rfl

/-- Only a transient answer or a quarantined id defers an agent. -/
theorem stepQ_skip {img : Bool} {rr : RR} {s : Streaks} (h : (stepQ img rr s).1 = .skip) :
    rr = .run (.err .unavailable) ∨ rr = .unknownFormat := by
  by_cases hq : rr = .unknownFormat
  · exact Or.inr hq
  · rw [stepQ_agrees img rr s hq] at h
    exact Or.inl (stepFixed_skip h)

/-- One serve: the fixed replay, then, unless it stopped, one more pass over
the rows it left owed, from the streaks `s'` (the Rust carries the first
pass's; the theorems take any). -/
def replayP (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks) (intents : List Intent) : ROut :=
  let r1 := replayD step live rout intents
  match r1.block with
  | some _ => r1
  | none =>
    let r2 := loopD step rout' s' [] (pendingOf (applyLog intents r1.log))
    { answers := r1.answers ++ r2.answers
      log := r1.log ++ r2.log
      applied := r1.applied ++ r2.applied
      failed := r1.failed ++ r2.failed
      block := r2.block }

/-- What the loop met at each intent it deferred an agent at, in order. -/
def skipsOf (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    Streaks → List Nat → List Intent → List RR
  | _, _, [] => []
  | s, d, i :: rest =>
    if i.job.agent ∈ d then skipsOf step rout s d rest
    else
      match step i.job.image (rout i.job) s with
      | (.applied, s') => skipsOf step rout s' d rest
      | (.failed, s') => skipsOf step rout s' d rest
      | (.skip, s') => rout i.job :: skipsOf step rout s' (i.job.agent :: d) rest
      | (.stop _, _) => []

/-- The serve after the remediation, with its block reason: when it ends with
writes owed and no stop, `embedder` if the last pass deferred an agent at a
transient answer, else `other` (only quarantined ids hold writes back). -/
def serveQ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := replayP stepQ live rout rout' s' intents
  if r.block = none ∧ pendingOf (applyLog intents r.log) ≠ [] then
    let left := pendingOf (applyLog intents (replayD stepQ live rout intents).log)
    { r with
      block := some (if (skipsOf stepQ rout' s' [] left).contains (.run (.err .unavailable))
        then .embedder else .other) }
  else r

/-- **The block reason is never null while writes are owed** after a serve
that did not stop: `write_queue_replay_blocked` names why. -/
theorem serveQ_block (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent)
    (h : pendingOf (applyLog intents (serveQ live rout rout' s' intents).log) ≠ []) :
    (serveQ live rout rout' s' intents).block ≠ none := by
  unfold serveQ at h ⊢
  by_cases hc : (replayP stepQ live rout rout' s' intents).block = none ∧
      pendingOf (applyLog intents (replayP stepQ live rout rout' s' intents).log) ≠ []
  · rw [ite_eq_left hc]; simp
  · rw [ite_eq_right hc] at h ⊢
    intro hb
    exact hc ⟨hb, h⟩

/-- The serve's writes and applies are the two passes'; only the block
reason is added. -/
theorem serveQ_log (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) :
    (serveQ live rout rout' s' intents).log = (replayP stepQ live rout rout' s' intents).log ∧
      (serveQ live rout rout' s' intents).applied =
        (replayP stepQ live rout rout' s' intents).applied := by
  unfold serveQ
  dsimp only
  split <;> exact ⟨rfl, rfl⟩

theorem applyLog_append (rows : List Intent) (l1 l2 : List Batch) :
    applyLog rows (l1 ++ l2) = applyLog (applyLog rows l1) l2 := by
  simp [applyLog, List.flatten_append, List.foldl_append]

theorem applyLog_keys (rows : List Intent) (log : List Batch) (hnp : NoPut log.flatten) :
    (applyLog rows log).map (·.job.key) = rows.map (·.job.key) := by
  have h := foldl_jobs log.flatten rows hnp
  have := congrArg (List.map (·.key)) h
  simpa [List.map_map, applyLog, Function.comp_def] using this

theorem applyLog_sorted (rows : List Intent) (log : List Batch) (hnp : NoPut log.flatten)
    (hs : EpochSorted rows) : EpochSorted (applyLog rows log) := by
  have h := foldl_jobs log.flatten rows hnp
  unfold EpochSorted at hs ⊢
  have hs' : (rows.map (·.job)).Pairwise (fun x y : Job => x.epoch = y.epoch → x.rid ≤ y.rid) :=
    List.pairwise_map.mpr hs
  rw [← show (applyLog rows log).map (·.job) = rows.map (·.job) from h] at hs'
  exact List.pairwise_map.mp hs'

/-- **A serve with the retry pass keeps the order invariant**: both passes
are runs of the fixed loop over per-process-sorted rows. -/
theorem replayP_preserves (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout rout' : Job → RR) (s' : Streaks) (rows : List Intent) (A : List Job)
    (hnd : (rows.map (·.job.key)).Nodup) (hs : EpochSorted rows) (hI : OrderInv rows A) :
    OrderInv (applyLog rows (replayP step live rout rout' s' rows).log)
      (A ++ (replayP step live rout rout' s' rows).applied) := by
  have h1 := replayD_preserves step live rout rows A hnd hs hI
  unfold replayP
  dsimp only
  cases hb : (replayD step live rout rows).block with
  | some b => exact h1
  | none =>
    dsimp only
    have hnp := (replayD_facts step live rout rows).2.2.1
    have hnd1 : ((applyLog rows (replayD step live rout rows).log).map (·.job.key)).Nodup := by
      rw [applyLog_keys _ _ hnp]; exact hnd
    have h2 := loopD_preserves step rout' s' _ _ hnd1 (applyLog_sorted _ _ hnp hs) h1
    rw [applyLog_append, ← List.append_assoc]
    exact h2

/-- With no step skipping or stopping, the fixed loop consumes every row it
is given. -/
theorem loopD_settles (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR)
    (hret : ∀ j s, (step j.image (rout j) s).1 = .applied ∨ (step j.image (rout j) s).1 = .failed) :
    ∀ (l : List Intent) (s : Streaks), ∀ i ∈ l,
      ∃ t, Mut.consume i.job.key t ∈ (loopD step rout s [] l).log.flatten := by
  intro l
  induction l with
  | nil => intro s i hi; simp at hi
  | cons i0 rest ih =>
    intro s i hi
    have hd : i0.job.agent ∉ ([] : List Nat) := by simp
    cases h : step i0.job.image (rout i0.job) s with
    | mk st s1 =>
      have hr := hret i0.job s
      rw [h] at hr
      rcases hr with hr | hr <;> simp only at hr <;> subst hr
      · rw [loopD_applied hd h]
        cases List.mem_cons.mp hi with
        | inl e => exact ⟨.appliedAfterRestart, by rw [e]; simp⟩
        | inr hi' =>
          obtain ⟨t, ht⟩ := ih s1 i hi'
          exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
      · rw [loopD_failed hd h]
        cases List.mem_cons.mp hi with
        | inl e => exact ⟨.failed, by rw [e]; simp⟩
        | inr hi' =>
          obtain ⟨t, ht⟩ := ih s1 i hi'
          exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩

/-- **Progress from the retry pass.** If the first pass did not stop and the
retry gets an answer for every write it runs (applied or refused: the
embedder answered and no id is quarantined), the serve leaves nothing owed.
What is not guaranteed: a write whose answer is transient at both attempts,
or a quarantined id, keeps its agent's writes owed to the next serve, every
serve if it never embeds (an owner question: an attempt bound). -/
theorem replayP_progress (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout rout' : Job → RR) (s' : Streaks) (rows : List Intent)
    (hb : (replayD step live rout rows).block = none)
    (hret : ∀ j s, (step j.image (rout' j) s).1 = .applied ∨ (step j.image (rout' j) s).1 = .failed) :
    pendingOf (applyLog rows (replayP step live rout rout' s' rows).log) = [] := by
  unfold replayP
  dsimp only
  rw [hb]
  dsimp only
  have hnp1 := (replayD_facts step live rout rows).2.2.1
  let rows1 := applyLog rows (replayD step live rout rows).log
  have hnp2 := (loopD_facts step rout' (pendingOf rows1) s' []).2.2.1
  rw [applyLog_append]
  apply List.filter_eq_nil_iff.mpr
  intro i' hi' hn
  have hio : i'.outcome = none := Option.isNone_iff_eq_none.mp hn
  apply foldl_consumed _ rows1 hnp2 ?_ i' hi' rfl hio
  by_cases hk : ∃ k ∈ rows1, k.job.key = i'.job.key ∧ k.outcome = none
  · obtain ⟨k, hk1, hkk, hko⟩ := hk
    have hkp : k ∈ pendingOf rows1 := List.mem_filter.mpr ⟨hk1, by simp [hko]⟩
    obtain ⟨t, ht⟩ := loopD_settles step rout' hret _ s' k hkp
    exact Or.inl ⟨t, hkk ▸ ht⟩
  · refine Or.inr ?_
    intro k hk' hkk hko
    exact hk ⟨k, hk', hkk, hko⟩

/-- **Per-agent order across the hold, a serve with its retry pass, and
quarantine**: the writes applied in session, then by the next serve (both of
its passes), keep each agent's writes of one process in admission order,
whatever the embedder answers. -/
theorem hold_then_serveQ_order (out : Nat → Job → RunResult) (now : Int) (s : RStore)
    (js : List Job) (e : Nat) (he : ∀ j ∈ js, j.epoch = e)
    (hjs : js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid)) (rows : List Intent)
    (hnd : (rows.map (·.job.key)).Nodup)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held)
    (live : Bool) (rout rout' : Job → RR) (s' : Streaks) :
    ((workH out now 0 (HState.init s) js).applied ++
        (serveQ live rout rout' s' (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hI := orderInv_perm (fixedLoad_perm rows) (session_orderInv out now s js e he hjs rows hrows)
  have hnd' : ((fixedLoad rows).map (·.job.key)).Nodup :=
    ((fixedLoad_perm rows).map _).nodup_iff.mpr hnd
  rw [(serveQ_log live rout rout' s' _).2]
  exact (replayP_preserves stepQ live rout rout' s' _ _ hnd' (fixedLoad_sorted rows) hI).1

/-! ## The hold with admission, caps, retries and the fence (#295 review)

`workH` treats a worker's whole pass as one step and has no admission, so it
could not show two faults review found: held writes counted against
admission while the only retry trigger was a new admitted write (they could
be stranded), and a retry ran several held writes after one lease check.
This model has the events themselves. `submit j`: admission, against the
caps, counting queued, running and held writes and their bytes. `start a`:
agent `a`'s worker takes one write: its oldest held write unless it is
parked after an outage, else its next queued write, which joins the held
ones if any are left. `finish a r`: that write's run returns `r`. `wake a`:
a drain-only retry (the backoff timer, a new write, or a refused admission
for an agent with held writes) ends the parking. `fence`: the lease is lost
(latched, as #293 makes it). Every step is atomic; a write in flight is
counted in `running`.

The fence is checked when a write is taken, before it runs: a write taken
after the fence is refused (`failed`, its intent left to the next holder)
and never run; a write already running when the fence lands can still
commit, which the store's fenced flush then refuses (`Flush.lean`).

Which errors hold a write (`holdsS`): an embedder outage. A lookup timeout
settles `failed`, as the replay settles a single one; the worker's run of
them that says the store is slow is handed to the model as `unavailable`
(the class the worker acts on, not its cause). The run counts first attempts
only: a held write's retry that times out again is handed over as
`lookupTimeout`, and settles.

**What the retry theorems cover, and what they do not.** `wake` and `start`
are free events of the environment, so `retry_settles_head` and
`retry_enabled` say that a retry needs no admission and is never blocked by
the caps (`stepW_caps_free`, which holds because only admission reads them),
and that it is always available while an agent has held writes and nothing
running. They do not say that a retry ever happens: that is the Rust's
backoff timer, which a worker keeps while its agent has held writes
(`execution::held_retry_backoff`), and timing is not modelled. Nor do they
bound how often it happens: in the Rust, neither a new write nor a refused
admission starts a retry before the backoff since the last attempt has
passed. Such a refusal is therefore no event in the model at all: it changes
nothing but the parking, which in the Rust it leaves alone. `refused_wakes`
describes the commit before that (`30b79824`), where a refusal woke the
retry at once. -/

/-- The admission bounds: per agent (`WRITE_QUEUE_LANE_MAX`), the whole queue
(`WRITE_QUEUE_MAX`), payload bytes (`WRITE_QUEUE_MAX_BYTES`). -/
structure Caps where
  lane : Nat
  total : Nat
  bytes : Nat

/-- An agent's writes in a list, in order. -/
def ofAgent (a : Nat) (l : List Job) : List Job := l.filter (fun k => k.agent = a)

/-- The list without the first write of agent `a`. -/
def dropFirst (a : Nat) : List Job → List Job
  | [] => []
  | x :: xs => if x.agent = a then xs else x :: dropFirst a xs

/-- The workers' state: receipts, queued, running and held writes, the
parked agents, the latched fence, the batches appended, what was settled (in
order), and every admitted write (in admission order). -/
structure WS where
  store : RStore
  queue : List Job
  running : List Job
  held : List Job
  parked : List Nat
  fenced : Bool
  log : List Batch
  settled : List (Job × Answer)
  admitted : List Job

def WS.init (s : RStore) : WS := ⟨s, [], [], [], [], false, [], [], []⟩

/-- What admission counts: queued, running and held writes. -/
def WS.load (st : WS) : List Job := st.queue ++ st.running ++ st.held

def WS.applied (st : WS) : List Job := (st.settled.filter (fun p => p.2 = .applied)).map (·.1)

inductive Ev where
  | submit (j : Job)
  | start (a : Nat)
  | finish (a : Nat) (r : RunResult)
  | wake (a : Nat)
  | fence
  deriving DecidableEq, Repr

/-- Admission's three checks, every one counting held writes. -/
def fits (c : Caps) (sz : Job → Nat) (st : WS) (j : Job) : Bool :=
  decide ((ofAgent j.agent st.load).length < c.lane) && decide (st.load.length < c.total) &&
    decide ((st.load.map sz).sum + sz j ≤ c.bytes)

/-- The errors that hold a write in session. -/
def holdsS (e : Err) : Bool := e == .unavailable

/-! The worker's moves, one write each. -/

/-- Take the oldest held write `h` to run. -/
def takeHeld (a : Nat) (h : Job) (st : WS) : WS :=
  { st with held := dropFirst a st.held, running := st.running ++ [h] }

/-- Take the next queued write `q` to run (nothing held). -/
def takeQueued (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, running := st.running ++ [q] }

/-- A parked agent's next queued write `q` joins its held writes. -/
def parkQueued (now : Int) (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, held := st.held ++ [q],
            store := settleOne st.store q.key .pendingReplay now }

/-- After the fence: refuse the oldest held write, never run. -/
def refuseHeld (now : Int) (a : Nat) (h : Job) (st : WS) : WS :=
  { st with held := dropFirst a st.held, settled := st.settled ++ [(h, .failed)],
            store := settleOne st.store h.key .failed now }

/-- After the fence: refuse the next queued write, never run. -/
def refuseQueued (now : Int) (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, settled := st.settled ++ [(q, .failed)],
            store := settleOne st.store q.key .failed now }

/-- The running write `j` settles with `x`, appending `b`. -/
def settleRun (now : Int) (a : Nat) (j : Job) (x : Answer) (b : Batch) (st : WS) : WS :=
  { st with running := dropFirst a st.running, settled := st.settled ++ [(j, x)],
            log := st.log ++ [b], store := settleOne st.store j.key x now }

/-- The running write `j` met an outage: it becomes the oldest held write and
the agent is parked until a retry wakes it. -/
def holdRun (now : Int) (a : Nat) (j : Job) (st : WS) : WS :=
  { st with running := dropFirst a st.running, held := j :: st.held, parked := a :: st.parked,
            store := settleOne st.store j.key .pendingReplay now }

def startW (now : Int) (a : Nat) (st : WS) : WS :=
  if ofAgent a st.running ≠ [] then st
  else
    match ofAgent a st.held, ofAgent a st.queue with
    | h :: _, q :: _ =>
      if st.fenced then refuseHeld now a h st
      else if a ∈ st.parked then parkQueued now a q st
      else takeHeld a h st
    | h :: _, [] =>
      if st.fenced then refuseHeld now a h st
      else if a ∈ st.parked then st
      else takeHeld a h st
    | [], q :: _ => if st.fenced then refuseQueued now a q st else takeQueued a q st
    | [], [] => st

def finishW (now : Int) (a : Nat) (r : RunResult) (st : WS) : WS :=
  match ofAgent a st.running with
  | [] => st
  | j :: _ =>
    match r with
    | .ok => settleRun now a j .applied [.write j.key, .consume j.key .applied] st
    | .err e =>
      if holdsS e then holdRun now a j st
      else settleRun now a j .failed [.consume j.key .failed] st

def stepW (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) : Ev → WS
  | .submit j =>
    if fits c sz st j then { st with queue := st.queue ++ [j], admitted := st.admitted ++ [j] }
    -- A refused write of an agent with held writes wakes its retry.
    else if ofAgent j.agent st.held ≠ [] then { st with parked := st.parked.filter (· ≠ j.agent) }
    else st
  | .start a => startW now a st
  | .finish a r => finishW now a r st
  | .wake a => { st with parked := st.parked.filter (· ≠ a) }
  | .fence => { st with fenced := true }

def runW (c : Caps) (sz : Job → Nat) (now : Int) : WS → List Ev → WS
  | st, [] => st
  | st, e :: es => runW c sz now (stepW c sz now st e) es

/-! ### Lemmas about agents' sub-lists -/

theorem ofAgent_append (a : Nat) (l l' : List Job) :
    ofAgent a (l ++ l') = ofAgent a l ++ ofAgent a l' := List.filter_append ..

theorem ofAgent_cons (a : Nat) (x : Job) (l : List Job) :
    ofAgent a (x :: l) = if x.agent = a then x :: ofAgent a l else ofAgent a l := by
  by_cases h : x.agent = a <;> simp [ofAgent, h]

theorem ofAgent_nil (a : Nat) : ofAgent a [] = [] := rfl

theorem mem_ofAgent {a : Nat} {l : List Job} {x : Job} (h : x ∈ ofAgent a l) :
    x ∈ l ∧ x.agent = a := by
  simpa [ofAgent] using h

theorem head_agent {a : Nat} {l t : List Job} {h : Job} (hl : ofAgent a l = h :: t) :
    h.agent = a :=
  (mem_ofAgent (by rw [hl]; exact List.mem_cons_self)).2

theorem ofAgent_single (b : Nat) (x : Job) :
    ofAgent b [x] = if x.agent = b then [x] else [] := by
  rw [ofAgent_cons, ofAgent_nil]

theorem dropFirst_spec (sz : Job → Nat) (a : Nat) : ∀ (l : List Job) (h : Job) (t : List Job),
    ofAgent a l = h :: t →
      ofAgent a (dropFirst a l) = t ∧ (∀ b, b ≠ a → ofAgent b (dropFirst a l) = ofAgent b l) ∧
      (dropFirst a l).length + 1 = l.length ∧
      ((dropFirst a l).map sz).sum + sz h = (l.map sz).sum := by
  intro l
  induction l with
  | nil => intro h t hl; simp [ofAgent_nil] at hl
  | cons x xs ih =>
    intro h t hl
    rw [ofAgent_cons] at hl
    by_cases hx : x.agent = a
    · simp only [hx, ite_true, List.cons.injEq] at hl
      obtain ⟨rfl, rfl⟩ := hl
      refine ⟨by simp [dropFirst, hx], ?_, by simp [dropFirst, hx], by simp [dropFirst, hx]; omega⟩
      intro b hb
      simp only [dropFirst, hx, ite_true, ofAgent_cons]
      simp [Ne.symm hb]
    · simp only [hx, ite_false] at hl
      obtain ⟨h1, h2, h3, h4⟩ := ih h t hl
      simp only [dropFirst, hx, ite_false]
      refine ⟨by rw [ofAgent_cons]; simp [hx, h1], ?_, by simp; omega, by simp; omega⟩
      intro b hb
      rw [ofAgent_cons, ofAgent_cons, h2 b hb]

/-! ### Per-agent order -/

/-- For every agent: what it settled, then the write it has running (at most
one), then its held writes, then its queued writes, are exactly its admitted
writes in admission order. -/
def OrdW (st : WS) : Prop :=
  ∀ b, ofAgent b (st.settled.map (·.1)) ++ ofAgent b st.running ++ ofAgent b st.held ++
      ofAgent b st.queue = ofAgent b st.admitted ∧ (ofAgent b st.running).length ≤ 1

theorem ordW_init (s : RStore) : OrdW (WS.init s) := by
  intro b; simp [WS.init, ofAgent_nil]

/-- The common shape of every move: agent `a`'s four sub-lists change so
that their concatenation does not, and every other agent's stay. -/
theorem ordW_move {st st' : WS} {a : Nat} (hI : OrdW st) (hadm : st'.admitted = st.admitted)
    (hother : ∀ b, b ≠ a → ofAgent b (st'.settled.map (·.1)) = ofAgent b (st.settled.map (·.1)) ∧
      ofAgent b st'.running = ofAgent b st.running ∧ ofAgent b st'.held = ofAgent b st.held ∧
      ofAgent b st'.queue = ofAgent b st.queue)
    (hself : ofAgent a (st'.settled.map (·.1)) ++ ofAgent a st'.running ++ ofAgent a st'.held ++
      ofAgent a st'.queue = ofAgent a (st.settled.map (·.1)) ++ ofAgent a st.running ++
        ofAgent a st.held ++ ofAgent a st.queue)
    (hrun : (ofAgent a st'.running).length ≤ 1) : OrdW st' := by
  intro b
  by_cases hb : b = a
  · subst hb; exact ⟨by rw [hself, hadm]; exact (hI b).1, hrun⟩
  · obtain ⟨o1, o2, o3, o4⟩ := hother b hb
    rw [o1, o2, o3, o4, hadm]
    exact hI b

theorem startW_ord (now : Int) (a : Nat) (st : WS) (hI : OrdW st) : OrdW (startW now a st) := by
  unfold startW
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hI
  · have hr' : ofAgent a st.running = [] := by simpa using hr
    rw [ite_eq_right hr]
    -- The four moves, each with agent `a`'s running sub-list empty.
    have tH : ∀ h t, ofAgent a st.held = h :: t → OrdW (takeHeld a h st) := by
      intro h t hh
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hh
      have ha := head_agent hh
      refine ordW_move hI rfl (fun b hb => ⟨rfl, ?_, d2 b hb, rfl⟩) ?_ ?_
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, Ne.symm hb]
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, hr', d1, hh]
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, hr']
    have rH : ∀ h t, ofAgent a st.held = h :: t → OrdW (refuseHeld now a h st) := by
      intro h t hh
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hh
      have ha := head_agent hh
      refine ordW_move hI rfl (fun b hb => ⟨?_, rfl, d2 b hb, rfl⟩) ?_ ?_
      · simp [refuseHeld, ofAgent_append, ofAgent_single, ha, Ne.symm hb]
      · simp [refuseHeld, ofAgent_append, ofAgent_single, ha, hr', d1, hh]
      · simp [refuseHeld, hr']
    have tQ : ∀ q t, ofAgent a st.held = [] → ofAgent a st.queue = q :: t →
        OrdW (takeQueued a q st) := by
      intro q t hh hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨rfl, ?_, rfl, d2 b hb⟩) ?_ ?_
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq, hh]
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, hr']
    have rQ : ∀ q t, ofAgent a st.held = [] → ofAgent a st.queue = q :: t →
        OrdW (refuseQueued now a q st) := by
      intro q t hh hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨?_, rfl, rfl, d2 b hb⟩) ?_ ?_
      · simp [refuseQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [refuseQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq, hh]
      · simp [refuseQueued, hr']
    have pQ : ∀ q t, ofAgent a st.queue = q :: t → OrdW (parkQueued now a q st) := by
      intro q t hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨rfl, rfl, ?_, d2 b hb⟩) ?_ ?_
      · simp [parkQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [parkQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq]
      · simp [parkQueued, hr']
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hI
      | cons q t =>
        simp only
        split
        · exact rQ q t hh hq
        · exact tQ q t hh hq
    | cons h t =>
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · exact rH h t hh
        · split
          · exact hI
          · exact tH h t hh
      | cons q t' =>
        simp only
        split
        · exact rH h t hh
        · split
          · exact pQ q t' hq
          · exact tH h t hh

theorem finishW_ord (now : Int) (a : Nat) (r : RunResult) (st : WS) (hI : OrdW st) :
    OrdW (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hI
  | cons j t =>
    have ja := head_agent hj
    have ht : t = [] := by
      have h2 := (hI a).2
      rw [hj] at h2
      cases t with
      | nil => rfl
      | cons _ _ => simp at h2
    subst ht
    obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.running j [] hj
    have sR : ∀ x bt, OrdW (settleRun now a j x bt st) := by
      intro x bt
      refine ordW_move hI rfl (fun b hb => ⟨?_, d2 b hb, rfl, rfl⟩) ?_ ?_
      · simp [settleRun, ofAgent_append, ofAgent_single, ja, Ne.symm hb]
      · simp [settleRun, ofAgent_append, ofAgent_single, ja, d1, hj]
      · simp [settleRun, d1]
    simp only
    cases r with
    | ok => exact sR _ _
    | err e =>
      simp only
      split
      · refine ordW_move hI rfl (fun b hb => ⟨rfl, d2 b hb, ?_, rfl⟩) ?_ ?_
        · simp [holdRun, ofAgent_cons, ja, Ne.symm hb]
        · simp [holdRun, ofAgent_cons, ja, d1, hj]
        · simp [holdRun, d1]
      · exact sR _ _

theorem stepW_ord (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) (hI : OrdW st) :
    OrdW (stepW c sz now st ev) := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · intro b
      obtain ⟨h1, h2⟩ := hI b
      refine ⟨?_, h2⟩
      simp only [ofAgent_append, ← h1, List.append_assoc]
    · split
      · exact hI
      · exact hI
  | wake a => exact hI
  | fence => exact hI
  | start a => exact startW_ord now a st hI
  | finish a r => exact finishW_ord now a r st hI

theorem runW_ord (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), OrdW st → OrdW (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih => intro st h; exact ih _ (stepW_ord c sz now st e h)

/-! ### Caps: held writes stay counted, bytes included -/

/-- Admission's bounds hold over everything it counts. -/
def CapsW (c : Caps) (sz : Job → Nat) (st : WS) : Prop :=
  (∀ b, (ofAgent b st.load).length ≤ c.lane) ∧ st.load.length ≤ c.total ∧
    (st.load.map sz).sum ≤ c.bytes

/-- A move that never adds to what admission counts keeps the caps: per
agent, in all, and in bytes. -/
theorem capsW_shrink {c : Caps} {sz : Job → Nat} {st st' : WS} (hC : CapsW c sz st)
    (h1 : ∀ b, (ofAgent b st'.load).length ≤ (ofAgent b st.load).length)
    (h2 : st'.load.length ≤ st.load.length) (h3 : (st'.load.map sz).sum ≤ (st.load.map sz).sum) :
    CapsW c sz st' :=
  ⟨fun b => Nat.le_trans (h1 b) (hC.1 b), Nat.le_trans h2 hC.2.1, Nat.le_trans h3 hC.2.2⟩

theorem startW_caps (c : Caps) (sz : Job → Nat) (now : Int) (a : Nat) (st : WS)
    (hC : CapsW c sz st) : CapsW c sz (startW now a st) := by
  unfold startW
  -- Moving the head `x` of one of agent `a`'s lists to another, or dropping it.
  have mv : ∀ (l : List Job) (x : Job) (t : List Job), ofAgent a l = x :: t →
      (∀ b, (ofAgent b (dropFirst a l)).length + (if x.agent = b then 1 else 0) =
        (ofAgent b l).length) ∧ (dropFirst a l).length + 1 = l.length ∧
      ((dropFirst a l).map sz).sum + sz x = (l.map sz).sum := by
    intro l x t hl
    obtain ⟨d1, d2, d3, d4⟩ := dropFirst_spec sz a l x t hl
    have xa := head_agent hl
    refine ⟨?_, d3, d4⟩
    intro b
    by_cases hb : b = a
    · subst hb; rw [d1, hl]; simp [xa]
    · rw [d2 b hb]; simp [xa, Ne.symm hb]
  have cnt : ∀ (b : Nat) (x : Job), (ofAgent b [x]).length = if x.agent = b then 1 else 0 := by
    intro b x; rw [ofAgent_single]; split <;> rfl
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hC
  · rw [ite_eq_right hr]
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hC
      | cons q t =>
        obtain ⟨m1, m2, m3⟩ := mv st.queue q t hq
        simp only
        split
        · apply capsW_shrink hC <;> (try intro b) <;>
            simp only [refuseQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
              List.sum_append]
          · have := m1 b; omega
          · omega
          · omega
        · apply capsW_shrink hC <;> (try intro b) <;>
            simp only [takeQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
              List.sum_append, cnt]
          · have := m1 b; omega
          · simp; omega
          · simp; omega
    | cons h t =>
      obtain ⟨n1, n2, n3⟩ := mv st.held h t hh
      have rH : CapsW c sz (refuseHeld now a h st) := by
        apply capsW_shrink hC <;> (try intro b) <;>
          simp only [refuseHeld, WS.load, ofAgent_append, List.length_append, List.map_append,
            List.sum_append]
        · have := n1 b; omega
        · omega
        · omega
      have tH : CapsW c sz (takeHeld a h st) := by
        apply capsW_shrink hC <;> (try intro b) <;>
          simp only [takeHeld, WS.load, ofAgent_append, List.length_append, List.map_append,
            List.sum_append, cnt]
        · have := n1 b; omega
        · simp; omega
        · simp; omega
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · exact rH
        · split
          · exact hC
          · exact tH
      | cons q t' =>
        obtain ⟨m1, m2, m3⟩ := mv st.queue q t' hq
        simp only
        split
        · exact rH
        · split
          · apply capsW_shrink hC <;> (try intro b) <;>
              simp only [parkQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
                List.sum_append, cnt]
            · have := m1 b; omega
            · simp; omega
            · simp; omega
          · exact tH

theorem finishW_caps (c : Caps) (sz : Job → Nat) (now : Int) (a : Nat) (r : RunResult) (st : WS)
    (hC : CapsW c sz st) : CapsW c sz (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hC
  | cons j t =>
    obtain ⟨d1, d2, d3, d4⟩ := dropFirst_spec sz a st.running j t hj
    have ja := head_agent hj
    have m1 : ∀ b, (ofAgent b (dropFirst a st.running)).length + (if j.agent = b then 1 else 0) =
        (ofAgent b st.running).length := by
      intro b
      by_cases hb : b = a
      · subst hb; rw [d1, hj]; simp [ja]
      · rw [d2 b hb]; simp [ja, Ne.symm hb]
    have sR : ∀ x bt, CapsW c sz (settleRun now a j x bt st) := by
      intro x bt
      apply capsW_shrink hC <;> (try intro b) <;>
        simp only [settleRun, WS.load, ofAgent_append, List.length_append, List.map_append,
          List.sum_append]
      · have := m1 b; omega
      · omega
      · omega
    simp only
    cases r with
    | ok => exact sR _ _
    | err e =>
      simp only
      split
      · apply capsW_shrink hC <;> (try intro b) <;>
          simp only [holdRun, WS.load, ofAgent_append, List.length_append,
            List.map_append, List.sum_append, List.length_cons, List.map_cons, List.sum_cons]
        · have hc : (ofAgent b (j :: st.held)).length =
              (ofAgent b st.held).length + (if j.agent = b then 1 else 0) := by
            rw [ofAgent_cons]; split <;> simp
          rw [hc]; have := m1 b; omega
        · omega
        · omega
      · exact sR _ _

/-- **The caps hold at every step**: queued, running and held writes, and
their bytes, never exceed the bounds. A held write stays counted from its
admission until it settles, through every retry, bytes included. -/
theorem stepW_caps (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (hC : CapsW c sz st) : CapsW c sz (stepW c sz now st ev) := by
  obtain ⟨cl, ct, cb⟩ := hC
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · rename_i hf
      simp only [fits, Bool.and_eq_true, decide_eq_true_eq] at hf
      obtain ⟨⟨f1, f2⟩, f3⟩ := hf
      refine ⟨?_, ?_, ?_⟩
      · intro b
        have := cl b
        by_cases hb : j.agent = b
        · subst hb
          simp only [WS.load, ofAgent_append, List.length_append] at f1 ⊢
          simp [ofAgent_single]; omega
        · simp only [WS.load, ofAgent_append, List.length_append] at this ⊢
          simp [ofAgent_single, hb]; omega
      · simp only [WS.load, List.length_append] at f2 ⊢; simp; omega
      · simp only [WS.load, List.map_append, List.sum_append] at f3 ⊢; simp; omega
    · split <;> exact ⟨cl, ct, cb⟩
  | wake a => exact ⟨cl, ct, cb⟩
  | fence => exact ⟨cl, ct, cb⟩
  | start a => exact startW_caps c sz now a st ⟨cl, ct, cb⟩
  | finish a r => exact finishW_caps c sz now a r st ⟨cl, ct, cb⟩

theorem runW_caps (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), CapsW c sz st → CapsW c sz (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih => intro st hC; exact ih _ (stepW_caps c sz now st e hC)

/-! ### Held writes are retried without any new admission -/

/-- **The caps cannot wedge a retry**: only admission reads them, so a retry
(`wake`, `start`, `finish`) is the same under any bounds. It holds by
definition; its point is that a retry never passes through admission. -/
theorem stepW_caps_free (c c' : Caps) (sz sz' : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (h : ∀ j, ev ≠ .submit j) : stepW c sz now st ev = stepW c' sz' now st ev := by
  cases ev with
  | submit j => exact absurd rfl (h j)
  | _ => rfl

/-- In the model, a refused admission for an agent with held writes wakes its
retry: what the Rust did at `30b79824`. Since the review that followed, the
Rust lets a refusal start no retry before the backoff has passed (see the
section's header). -/
theorem refused_wakes (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (j : Job)
    (hf : fits c sz st j = false) (hh : ofAgent j.agent st.held ≠ []) :
    j.agent ∉ (stepW c sz now st (.submit j)).parked := by
  simp [stepW, hf, hh]

/-- **A retry needs no admission**: an agent with held writes and nothing
running, once its retry is due (`wake`), settles its oldest held write when
the embedder answers it, with no admission among the events. That the retry
comes due is the Rust's timer, not this theorem. -/
theorem retry_settles_head (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat)
    (h : Job) (t : List Job) (r : RunResult) (hheld : ofAgent a st.held = h :: t)
    (hrun : ofAgent a st.running = []) (hf : st.fenced = false)
    (hr : ∀ e, r = .err e → holdsS e = false) :
    ofAgent a (runW c sz now st [.wake a, .start a, .finish a r]).held = t ∧
      (runW c sz now st [.wake a, .start a, .finish a r]).settled =
        st.settled ++ [(h, if r = .ok then .applied else .failed)] := by
  obtain ⟨d1, _, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hheld
  have ha := head_agent hheld
  have hs : startW now a { st with parked := st.parked.filter (· ≠ a) } =
      takeHeld a h { st with parked := st.parked.filter (· ≠ a) } := by
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hrun]
    rw [ite_eq_right hn]
    simp only [hheld, hf, Bool.false_eq_true, ite_false]
    cases hq : ofAgent a st.queue <;> simp
  have hr2 : ofAgent a (st.running ++ [h]) = [h] := by
    simp [ofAgent_append, hrun, ofAgent_single, ha]
  simp only [runW, stepW]
  rw [hs]
  cases r with
  | ok => simp [finishW, takeHeld, settleRun, hr2, d1]
  | err e =>
    have he := hr e rfl
    simp [finishW, takeHeld, settleRun, hr2, d1, he]

/-- **A retry is always available while writes are held**: for an agent
with held writes, nothing running and the lease held, the events `wake` then
`start` take its oldest held write to run, whatever the parking, the caps or
the rest of the state. (Enabledness, not fairness: that the environment
delivers `wake` is the Rust's timer.) -/
theorem retry_enabled (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat) (h : Job)
    (t : List Job) (hheld : ofAgent a st.held = h :: t) (hrun : ofAgent a st.running = [])
    (hf : st.fenced = false) :
    (runW c sz now st [.wake a, .start a]).running = st.running ++ [h] ∧
      ofAgent a (runW c sz now st [.wake a, .start a]).held = t := by
  obtain ⟨d1, _, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hheld
  have hs : startW now a { st with parked := st.parked.filter (· ≠ a) } =
      takeHeld a h { st with parked := st.parked.filter (· ≠ a) } := by
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hrun]
    rw [ite_eq_right hn]
    simp only [hheld, hf, Bool.false_eq_true, ite_false]
    cases hq : ofAgent a st.queue <;> simp
  simp only [runW, stepW]
  rw [hs]
  exact ⟨rfl, d1⟩

/-! ### The fence -/

theorem stepW_fenced (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (hf : st.fenced = true) (hr : st.running = []) :
    (stepW c sz now st ev).fenced = true ∧ (stepW c sz now st ev).running = [] ∧
      (stepW c sz now st ev).applied = st.applied ∧ (stepW c sz now st ev).log = st.log := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact ⟨hf, hr, rfl, rfl⟩
    · split
      · exact ⟨hf, hr, rfl, rfl⟩
      · exact ⟨hf, hr, rfl, rfl⟩
  | wake a => exact ⟨hf, hr, rfl, rfl⟩
  | fence => exact ⟨rfl, hr, rfl, rfl⟩
  | finish a r => simp [stepW, finishW, hr, ofAgent_nil, hf]
  | start a =>
    simp only [stepW]
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hr, ofAgent_nil]
    rw [ite_eq_right hn]
    cases hh : ofAgent a st.held <;> cases hq : ofAgent a st.queue <;>
      simp [refuseHeld, refuseQueued, WS.applied, hf, hr]

/-- **No write taken after the fence is applied.** Once the lease is lost
(latched) and nothing is running, no later event applies or logs anything:
every held or queued write is refused, oldest first, without being run. A
write already running when the fence landed is the only one that can still
commit, and the store refuses that flush. -/
theorem fence_stops_applies (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), st.fenced = true → st.running = [] →
      (runW c sz now st evs).applied = st.applied ∧ (runW c sz now st evs).log = st.log := by
  intro evs
  induction evs with
  | nil => intro st _ _; exact ⟨rfl, rfl⟩
  | cons e es ih =>
    intro st hf hr
    obtain ⟨f1, f2, f3, f4⟩ := stepW_fenced c sz now st e hf hr
    obtain ⟨g1, g2⟩ := ih _ f1 f2
    exact ⟨g1.trans f3, g2.trans f4⟩

/-! ### Order across the session and the next serve -/

/-- **Per-agent order in session, with admission and retries.** The writes
applied in session satisfy the order invariant against any rows whose owed
writes of this process are still running, held or queued. -/
theorem sessionW_orderInv (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore) (evs : List Ev)
    (e : Nat) (he : ∀ j ∈ (runW c sz now (WS.init s) evs).admitted, j.epoch = e)
    (hjs : (runW c sz now (WS.init s) evs).admitted.Pairwise
      (fun a b => a.agent = b.agent → a.rid < b.rid))
    (rows : List Intent)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (runW c sz now (WS.init s) evs).running ∨
      i.job ∈ (runW c sz now (WS.init s) evs).held ∨
      i.job ∈ (runW c sz now (WS.init s) evs).queue) :
    OrderInv rows (runW c sz now (WS.init s) evs).applied := by
  let st := runW c sz now (WS.init s) evs
  have hO : OrdW st := runW_ord c sz now evs _ (ordW_init s)
  have hsub : st.applied.Sublist (st.settled.map (·.1)) := (List.filter_sublist).map _
  have hja : ∀ a, (ofAgent a st.admitted).Pairwise (fun x y => x.rid < y.rid) := by
    intro a
    apply List.Pairwise.imp_of_mem _ (hjs.sublist List.filter_sublist)
    intro x y hx hy hxy
    exact hxy (by rw [(mem_ofAgent hx).2, (mem_ofAgent hy).2])
  have hsettled_mem : ∀ x ∈ st.settled.map (·.1), x ∈ st.admitted := by
    intro x hx
    have h1 : x ∈ ofAgent x.agent (st.settled.map (·.1)) := by simp [ofAgent, hx]
    have h2 := (hO x.agent).1
    have : x ∈ ofAgent x.agent st.admitted := by
      rw [← h2]; simp only [List.append_assoc]; exact List.mem_append_left _ h1
    exact (mem_ofAgent this).1
  refine ⟨?_, ?_⟩
  · apply List.Pairwise.sublist hsub
    apply pairwise_of_agent_filters
    intro a
    have hpre : (ofAgent a (st.settled.map (·.1))).Sublist (ofAgent a st.admitted) := by
      rw [← (hO a).1]; simp only [List.append_assoc]; exact List.sublist_append_left _ _
    exact ((hja a).sublist hpre).imp
      (fun {x y} (h : x.rid < y.rid) (_ : x.epoch = y.epoch) => Nat.le_of_lt h)
  · intro x hx i hi hio hag hep
    have hxs := hsub.subset hx
    have hxe : x.epoch = e := he x (hsettled_mem x hxs)
    have hpa := hja x.agent
    rw [← (hO x.agent).1] at hpa
    simp only [List.append_assoc] at hpa
    have hcross := (List.pairwise_append.mp hpa).2.2
    have hxa : x ∈ ofAgent x.agent (st.settled.map (·.1)) := by simp [ofAgent, hxs]
    have hio' : i.job ∈ ofAgent x.agent st.running ++ (ofAgent x.agent st.held ++
        ofAgent x.agent st.queue) := by
      have hm : ∀ l : List Job, i.job ∈ l → i.job ∈ ofAgent x.agent l :=
        fun l h => List.mem_filter.mpr ⟨h, by simp [hag]⟩
      rcases hrows i hi hio (hep ▸ hxe) with h | h | h
      · exact List.mem_append_left _ (hm _ h)
      · exact List.mem_append_right _ (List.mem_append_left _ (hm _ h))
      · exact List.mem_append_right _ (List.mem_append_right _ (hm _ h))
    exact Nat.le_of_lt (hcross x hxa i.job hio')

/-! ### The replay's block reason after a halt -/

/-- What `write_queue_replay_blocked` reports for a block reason: `unset` (a
halt on a lost lease or a seal) reads `null`, like no block. -/
def reported : Option Block → Option Block
  | some .unset => none
  | b => b

/-- The serve after the review: a halt that leaves intents owed reports
`other`, as a lost lease is an `other` fault. -/
def serveR (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := serveQ live rout rout' s' intents
  if r.block = some .unset ∧ pendingOf (applyLog intents r.log) ≠ [] then
    { r with block := some .other }
  else r

/-- **The reported block reason is never `null` while intents are owed**,
halts included (`serveQ_block` holds only before `unset` is read as `null`). -/
theorem serveR_reported (live : Bool) (rout rout' : Job → RR) (s' : Streaks)
    (intents : List Intent)
    (h : pendingOf (applyLog intents (serveR live rout rout' s' intents).log) ≠ []) :
    reported (serveR live rout rout' s' intents).block ≠ none := by
  unfold serveR at h ⊢
  dsimp only at h ⊢
  by_cases hc : (serveQ live rout rout' s' intents).block = some .unset ∧
      pendingOf (applyLog intents (serveQ live rout rout' s' intents).log) ≠ []
  · rw [ite_eq_left hc]; simp [reported]
  · rw [ite_eq_right hc] at h ⊢
    have hb := serveQ_block live rout rout' s' intents h
    cases hq : (serveQ live rout rout' s' intents).block with
    | none => exact absurd hq hb
    | some b =>
      cases b with
      | unset => exact absurd ⟨hq, h⟩ hc
      | _ => simp [reported]

theorem serveR_log (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) :
    (serveR live rout rout' s' intents).log = (serveQ live rout rout' s' intents).log ∧
      (serveR live rout rout' s' intents).applied = (serveQ live rout rout' s' intents).applied := by
  unfold serveR
  dsimp only
  split <;> exact ⟨rfl, rfl⟩

/-- **Per-agent order from the session (admission, caps, retries) through
the next serve (both passes)**, whatever the embedder answers. -/
theorem sessionW_then_serve_order (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore)
    (evs : List Ev) (e : Nat)
    (he : ∀ j ∈ (runW c sz now (WS.init s) evs).admitted, j.epoch = e)
    (hjs : (runW c sz now (WS.init s) evs).admitted.Pairwise
      (fun a b => a.agent = b.agent → a.rid < b.rid))
    (rows : List Intent) (hnd : (rows.map (·.job.key)).Nodup)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (runW c sz now (WS.init s) evs).running ∨
      i.job ∈ (runW c sz now (WS.init s) evs).held ∨
      i.job ∈ (runW c sz now (WS.init s) evs).queue)
    (live : Bool) (rout rout' : Job → RR) (s' : Streaks) :
    ((runW c sz now (WS.init s) evs).applied ++
        (serveR live rout rout' s' (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hI := orderInv_perm (fixedLoad_perm rows)
    (sessionW_orderInv c sz now s evs e he hjs rows hrows)
  have hnd' : ((fixedLoad rows).map (·.job.key)).Nodup :=
    ((fixedLoad_perm rows).map _).nodup_iff.mpr hnd
  rw [(serveR_log live rout rout' s' _).2, (serveQ_log live rout rout' s' _).2]
  exact (replayP_preserves stepQ live rout rout' s' _ _ hnd' (fixedLoad_sorted rows) hI).1

/-! ## Crash: a durable write is never replayed (worker before #295)

Over `work` and the replay before #294 and #295 (`replay`, `loadOrder`). The
current counterpart, over `runW` and `serveR` with `fixedLoad`, is
`crash_never_reappliesW`. -/

theorem mem_admit_flatten {all : List Job} {n : Nat} {m : Mut}
    (hm : m ∈ ((admitLog all).take n).flatten) : ∃ j, m = .put j := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j, _, rfl⟩ := List.mem_map.mp ((List.take_sublist n _).subset hb)
  exact ⟨j, List.mem_singleton.mp hmb⟩

/-- A mutation in a prefix of `work`'s batches (the worker before #295) is in
one finished job's `workerStep` batch. Current counterpart: `runW_log_refines`. -/
theorem mem_work_flatten {fence : Job → Bool} {out : Job → RunResult} {now : Int} {s : RStore}
    {done : List Job} {k : Nat} {m : Mut}
    (hm : m ∈ ((work fence out now s done).2.take k).flatten) :
    ∃ d ∈ done, m ∈ (workerStep (fence d) d (out d)).2 := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  have hb' := (List.take_sublist k _).subset hb
  rw [work_log] at hb'
  obtain ⟨d, hd, rfl⟩ := List.mem_map.mp hb'
  exact ⟨d, hd, hmb⟩

/-- `work`'s batches (the worker before #295) carry no put. The current
worker's do not either: `runW_log_refines` with `workerStep_keys`. -/
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
(`Graph::consume_write_intent`'s doc, `WriteCtx::run`'s doc). Stated over the
worker **before #295** (`work`) and the one-pass replay before #294
(`replay`, `loadOrder`). Current counterpart: `crash_never_reappliesW`, over
`runW` and `serveR` (derived through `runW_log_refines`). -/
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

/-! ## Shutdown: drained or persisted, never silently lost (worker before #295)

Over `work` and `close`, the worker **before #295**. The current
counterparts are in the section "The close, crash and shutdown over the
current worker": `shutdown_no_silent_lossW`, `held_at_closeW`,
`outage_holdsW`. -/

/-- A job whose own batch consumes nothing, and whose number no other
finished job shares, is still owed in the log: its row is unconsumed. Over
`work`, the worker **before #295**; current counterpart: `owedW_of` with
`InvW` (a running, held, queued or fenced write's intent has no consume in the
log), as used by `shutdown_no_silent_lossW`. -/
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
for ("Acked ⇒ (applied ∨ durable intent) at a clean close", `quiesce`).
Stated over the worker **before #295** (`work`, `workerStep`: an in-session
outage answered `failed`, so this theorem has no held writes in it). Current
counterpart: `shutdown_no_silent_lossW`, over `runW` and `closeW`, held,
queued, running and fenced writes included. -/
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

/-- **Before #295: an in-session outage answered `failed` and left the write
owed.** A job the worker ran into `EmbedUnavailable` (an outage or a lookup
timeout) is answered `failed`, but its intent is left unconsumed (by design,
so that an outage does not destroy it), so the next serve's replay owes it
and, if the embedder answers then, applies it. See
`outage_failed_then_applied` for the receipt this produces.

**The current Rust does not behave this way.** Since #295 the worker holds a
write that meets an outage: it answers `pending_replay`, not `failed`, keeps
its intent owed, holds the agent's later writes behind it, and is retried in
the background; a close answers it `intent_durable` (or `close_unflushed`). A
single lookup timeout now settles `failed` with its intent consumed, and the
one that ends a run of them is held. This theorem is kept because it
documents the behaviour #295 replaced: an owed write answered `failed`, which
let the agent's later writes overtake it (`outage_then_replay_reorders`).
Current counterparts: `outage_holdsW`, `held_at_closeW`,
`lookupTimeout_settlesW`, `lookupTimeout_run_holdsW`. -/
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

/-! ## The close, crash and shutdown over the current worker (#295 on)

The close, crash and shutdown theorems earlier in this file (`close`,
`close_settles_all`, `close_answer`, `owed_in_log`, `shutdown_no_silent_loss`,
`crash_never_reapplies`, `outage_failed_but_owed`) are stated over the worker
before #295 (`work`, `workerStep`), under which an in-session embedder outage
answers `failed`. That worker no longer exists. This section states them
again over the current one (`stepW`, `runW`: admission, held writes,
retries, the fence) and the current serve (`serveR` over `fixedLoad`).

Rust: `quiesce`, `abort_workers`, `settle_deferred` (`src/writeq/drain.rs`);
`WritePipeline::spawn_worker` (`src/writeq/execution.rs`, the hold arm and
`lookup_streak`); `Lanes::held` (`src/writeq/admission.rs`); `Memory::close`
and `Memory::close_fenced` (`src/memory/shutdown.rs`), which call
`settle_deferred` with `CloseFlush::Durable` only once the final flush has
completed (or found an empty tail), and with `Incomplete` when it fails, when
the session is degraded, or when the lease was lost.

What the close does to an admitted write, by its state when `close()` starts:

* **applied** (`settleRun` on `.ok`): settled `applied`, its write and its
  consume in one batch; `settle_deferred` leaves it alone.
* **failed and consumed** (`settleRun` on an error that does not hold it: a
  content or input refusal, a single lookup timeout, or the retry of a write
  held by a run of them): settled `failed`, intent consumed `failed`; left
  alone.
* **fenced** (`refuseHeld`, `refuseQueued`: taken after the lease was lost):
  settled `failed` without being run, its intent left unconsumed for the
  session's current holder; left alone. A fenced close refuses to flush
  (`close_fenced`), so everything it sets aside answers `close_unflushed`.
* **held** (`holdRun`, `parkQueued`: an embedder outage, or the third lookup
  timeout in a row with nothing applied between, or a later write of a held
  agent): `pending_replay`, unsettled, intent unconsumed. `abort_workers`
  clears `Lanes::held` and sets the receipt aside; `settle_deferred` answers
  `intent_durable` after a completed final flush, `close_unflushed` otherwise.
  It never answers `failed` in session.
* **queued, not started**: the same. `abort_workers` drains the queues and
  sets the receipts aside.
* **running when the close starts**: the quiesce waits for it, at most
  `WRITE_QUEUE_DRAIN_BUDGET`. If it finishes in that time it is one of the
  states above. If not, it is aborted at an `.await` before its commit (there
  is none between a commit and its settle), so it wrote nothing, its receipt
  is still unsettled, and it is set aside like a queued write.

In the model the drain budget is the trace: `closeW` is taken after any
`runW` trace, so whatever the workers finished before the abort is in it, and
a write still in `running` is one the abort stopped. `closeW` sets aside every
unsettled receipt (`deferredW`, as `abort_workers` does) and settles them by
`CloseFlush` (`settleDeferred`, shared with the old layer, as
`settle_deferred` is). The flush outcome is a parameter, as for `close`. A
retried close settles the same set-aside ids again (`close_deferred` is kept
after an incomplete flush).

`InvW` carries the argument: each settled write's receipt holds its answer
and the log its batch (or, refused after the fence, no consume); each queued
or running write's receipt is unsettled; each held write answers
`pending_replay`; none of these has a consume in the log; and a write not yet
admitted is still `pending`. It needs the admitted writes' receipt ids
distinct and every admitted write among the jobs whose intents admission
logged (`admitLog all`). The log has the shape `crash_never_reapplies` takes:
admission's puts, then the workers' batches.

Results: `shutdown_no_silent_lossW` (after a close every admitted write is
exactly one of applied, failed and consumed, failed after the fence with its
intent left owed, `intent_durable` and owed in the flushed log, or
`close_unflushed`; none pending), `held_at_closeW` (a held write answers
`pending_replay` and is owed while the session runs, and the close answers it
`intent_durable` or `close_unflushed`), `outage_holdsW` (an outage holds the
write: the current counterpart of `outage_failed_but_owed`),
`lookupTimeout_settlesW`, `lookupTimeout_run_holdsW`, `heldErrW_holds` (one
lookup timeout settles `failed` and consumes; a run of them holds),
`closeW_retry_upgradesW` (a retried close upgrades `close_unflushed`),
`runW_log_refines` (every batch the current worker appends is one the old
`workerStep` appends), `serveR_applied_pending` (the current serve applies only
stored, unconsumed rows), and `crash_never_reappliesW` (no write in a durable
prefix of the current worker's log is applied again by the current serve). -/

/-- The receipts `abort_workers` sets aside: every one still unsettled once the
workers are aborted and joined (`pending` or `pending_replay`), held writes'
included. -/
def deferredW (st : WS) : List Key := pendingIds st.store

/-- `close()` over the current worker: `quiesce` (the trace `st` came from is
what the workers finished; `running`, `held` and `queue` are what the abort
stops), `abort_workers`, then `settle_deferred` by how the final flush ended.
It appends nothing to the log. -/
def closeW (flush : CloseFlush) (now : Int) (st : WS) : RStore :=
  settleDeferred flush now (deferredW st) st.store

/-- The answer a receipt id holds in a store. -/
def ansOf (s : RStore) (k : Key) : Option Answer := (lookup s k).map (·.answer)

/-- No batch of the log consumes `k`. -/
def Untouched (k : Key) (log : List Batch) : Prop := ∀ t, Mut.consume k t ∉ log.flatten

/-- `j`'s intent is an unconsumed row of the session's whole log, admission's
puts then the workers' batches, in the current load order (`fixedLoad`): owed
by the next serve once that log is durable. -/
def OwedW (all : List Job) (st : WS) (j : Job) : Prop :=
  ⟨j, none⟩ ∈ pendingOf (fixedLoad (intentsOf (admitLog all ++ st.log).flatten))

/-- Every write the workers hold: settled, running, held, queued. -/
def DW (st : WS) : List Job := st.settled.map (·.1) ++ st.running ++ st.held ++ st.queue

/-! ### Distinct receipt ids across the workers' lists -/

theorem nodup_of_agents : ∀ (l : List Job), (∀ b, (ofAgent b l).Nodup) → l.Nodup := by
  intro l
  induction l with
  | nil => intro _; exact List.nodup_nil
  | cons x xs ih =>
    intro h
    have hx := h x.agent
    rw [ofAgent_cons, ite_eq_left rfl, List.nodup_cons] at hx
    refine List.nodup_cons.mpr ⟨?_, ih ?_⟩
    · intro hm; exact hx.1 (List.mem_filter.mpr ⟨hm, by simp⟩)
    · intro b
      have hb := h b
      rw [ofAgent_cons] at hb
      split at hb
      · exact (List.nodup_cons.mp hb).2
      · exact hb

theorem nodup_keys_sub {m : List Job} (hk : (m.map (·.key)).Nodup) :
    ∀ {l : List Job}, l.Nodup → (∀ x ∈ l, x ∈ m) → (l.map (·.key)).Nodup := by
  intro l
  induction l with
  | nil => intro _ _; exact List.nodup_nil
  | cons x xs ih =>
    intro hl hm
    rw [List.nodup_cons] at hl
    rw [List.map_cons, List.nodup_cons]
    refine ⟨?_, ih hl.2 (fun y hy => hm y (List.mem_cons_of_mem _ hy))⟩
    intro hx
    obtain ⟨y, hy, hyx⟩ := List.mem_map.mp hx
    have := eq_of_key hk (hm y (List.mem_cons_of_mem _ hy)) (hm x List.mem_cons_self) hyx
    exact hl.1 (this ▸ hy)

theorem dropFirst_perm (a : Nat) : ∀ (l : List Job) (h : Job) (t : List Job),
    ofAgent a l = h :: t → (h :: dropFirst a l).Perm l := by
  intro l
  induction l with
  | nil => intro h t hl; simp [ofAgent_nil] at hl
  | cons x xs ih =>
    intro h t hl
    rw [ofAgent_cons] at hl
    by_cases hx : x.agent = a
    · simp only [hx, ite_true, List.cons.injEq] at hl
      obtain ⟨rfl, rfl⟩ := hl
      simp only [dropFirst, hx, ite_true]
      exact List.Perm.refl _
    · simp only [hx, ite_false] at hl
      simp only [dropFirst, hx, ite_false]
      exact (List.Perm.swap x h _).trans ((ih h t hl).cons x)

theorem mem_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t)
    {x : Job} (hx : x ∈ dropFirst a l) : x ∈ l :=
  (dropFirst_perm a l h t hl).mem_iff.mp (List.mem_cons_of_mem _ hx)

theorem head_mem {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t) :
    h ∈ l :=
  (mem_ofAgent (by rw [hl]; exact List.mem_cons_self)).1

theorem key_ne_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job}
    (hl : ofAgent a l = h :: t) (hk : (l.map (·.key)).Nodup) {x : Job} (hx : x ∈ dropFirst a l) :
    x.key ≠ h.key := by
  have hp := (dropFirst_perm a l h t hl).map (·.key)
  have hn := (List.nodup_cons.mp (hp.nodup_iff.mpr hk)).1
  intro he
  exact hn (by show h.key ∈ _; rw [← he]; exact List.mem_map_of_mem hx)

/-- What distinct receipt ids give: the four lists' ids are distinct, within
and across lists, and every write in them was admitted. -/
structure KeysW (st : WS) : Prop where
  run : (st.running.map (·.key)).Nodup
  held : (st.held.map (·.key)).Nodup
  queue : (st.queue.map (·.key)).Nodup
  sr : ∀ p ∈ st.settled, ∀ y ∈ st.running, p.1.key ≠ y.key
  sh : ∀ p ∈ st.settled, ∀ y ∈ st.held, p.1.key ≠ y.key
  sq : ∀ p ∈ st.settled, ∀ y ∈ st.queue, p.1.key ≠ y.key
  rh : ∀ y ∈ st.running, ∀ z ∈ st.held, y.key ≠ z.key
  rq : ∀ y ∈ st.running, ∀ z ∈ st.queue, y.key ≠ z.key
  hq : ∀ y ∈ st.held, ∀ z ∈ st.queue, y.key ≠ z.key
  adm : ∀ x ∈ DW st, x ∈ st.admitted

theorem mem_DW_of_admitted {st : WS} (hO : OrdW st) {j : Job} (hj : j ∈ st.admitted) :
    j ∈ DW st := by
  have h1 : j ∈ ofAgent j.agent st.admitted := List.mem_filter.mpr ⟨hj, by simp⟩
  rw [← (hO j.agent).1] at h1
  simp only [DW] at h1 ⊢
  simp only [List.mem_append] at h1 ⊢
  rcases h1 with ((h | h) | h) | h
  · exact Or.inl (Or.inl (Or.inl (mem_ofAgent h).1))
  · exact Or.inl (Or.inl (Or.inr (mem_ofAgent h).1))
  · exact Or.inl (Or.inr (mem_ofAgent h).1)
  · exact Or.inr (mem_ofAgent h).1

theorem keysW_of (st : WS) (hO : OrdW st) (hnd : (st.admitted.map (·.key)).Nodup) : KeysW st := by
  have hadm : st.admitted.Nodup :=
    List.Pairwise.of_map (·.key) (fun _ _ h e => h (e ▸ rfl)) hnd
  have hsub : ∀ x ∈ DW st, x ∈ st.admitted := by
    intro x hx
    have h1 : x ∈ ofAgent x.agent (DW st) := List.mem_filter.mpr ⟨hx, by simp⟩
    have h2 : ofAgent x.agent (DW st) = ofAgent x.agent st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO x.agent).1
    rw [h2] at h1
    exact (mem_ofAgent h1).1
  have hD : (DW st).Nodup := by
    apply nodup_of_agents
    intro b
    have h2 : ofAgent b (DW st) = ofAgent b st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO b).1
    rw [h2]
    exact hadm.sublist List.filter_sublist
  have hK := nodup_keys_sub hnd hD hsub
  unfold DW at hK
  rw [List.map_append, List.map_append, List.map_append] at hK
  obtain ⟨h1, hQ, x1⟩ := List.nodup_append.mp hK
  obtain ⟨h2, hH, x2⟩ := List.nodup_append.mp h1
  obtain ⟨_, hR, x3⟩ := List.nodup_append.mp h2
  have mS : ∀ p ∈ st.settled, p.1.key ∈ (st.settled.map (·.1)).map (·.key) :=
    fun p hp => List.mem_map_of_mem (List.mem_map_of_mem hp)
  have mk : ∀ {l : List Job} {y : Job}, y ∈ l → y.key ∈ l.map (·.key) :=
    fun hy => List.mem_map_of_mem hy
  refine ⟨hR, hH, hQ, ?_, ?_, ?_, ?_, ?_, ?_, hsub⟩
  · intro p hp y hy; exact x3 _ (mS p hp) _ (mk hy)
  · intro p hp y hy; exact x2 _ (List.mem_append_left _ (mS p hp)) _ (mk hy)
  · intro p hp y hy
    exact x1 _ (List.mem_append_left _ (List.mem_append_left _ (mS p hp))) _ (mk hy)
  · intro y hy z hz; exact x2 _ (List.mem_append_right _ (mk hy)) _ (mk hz)
  · intro y hy z hz
    exact x1 _ (List.mem_append_left _ (List.mem_append_right _ (mk hy))) _ (mk hz)
  · intro y hy z hz; exact x1 _ (List.mem_append_right _ (mk hy)) _ (mk hz)

/-! ### The receipts and the log, as the workers leave them -/

theorem ansOf_settleOne_ne {s : RStore} {k q : Key} (x : Answer) (now : Int) (h : q ≠ k) :
    ansOf (settleOne s k x now) q = ansOf s q := by
  unfold ansOf settleOne; rw [lookup_update_ne s _ h]

theorem ansOf_settleOne_self {s : RStore} {k : Key} {a : Answer} (x : Answer) (now : Int)
    (ha : ansOf s k = some a) (hu : a.isSettled = false) :
    ansOf (settleOne s k x now) k = some x := by
  unfold ansOf at ha ⊢
  unfold settleOne
  rw [lookup_update, ite_eq_left rfl]
  cases hl : lookup s k with
  | none => rw [hl] at ha; simp at ha
  | some e =>
    rw [hl] at ha
    have he : e.answer = a := by simpa using ha
    have hs : ¬ e.answer.isSettled = true := by rw [he, hu]; decide
    simp [Lambo.Receipts.settle_unsettled e x now hs]

theorem untouched_snoc {q k : Key} {log : List Batch} {b : Batch} (hq : q ≠ k)
    (hb : ∀ m ∈ b, m = .write k ∨ ∃ t, m = .consume k t) (hu : Untouched q log) :
    Untouched q (log ++ [b]) := by
  intro t ht
  rw [List.flatten_append, List.mem_append] at ht
  cases ht with
  | inl h => exact hu t h
  | inr h =>
    simp only [List.flatten_cons, List.flatten_nil, List.append_nil] at h
    rcases hb _ h with h' | ⟨t', h'⟩
    · cases h'
    · exact hq (Mut.consume.inj h').1

/-- What every reachable state of the current workers keeps: each settled
write's receipt holds its answer and its batch (or, refused after the fence,
no consume at all); each queued or running write's receipt is unsettled and
its intent untouched; each held write answers `pending_replay` with its
intent untouched; and a write not yet admitted is still `pending` and
untouched. -/
structure InvW (all : List Job) (st : WS) : Prop where
  settled : ∀ p ∈ st.settled, ansOf st.store p.1.key = some p.2 ∧
    ((p.2 = .applied ∧ [Mut.write p.1.key, .consume p.1.key .applied] ∈ st.log) ∨
     (p.2 = .failed ∧ [Mut.consume p.1.key .failed] ∈ st.log) ∨
     (p.2 = .failed ∧ st.fenced = true ∧ Untouched p.1.key st.log))
  live : ∀ j ∈ st.running ++ st.queue,
    (∃ a, ansOf st.store j.key = some a ∧ a.isSettled = false) ∧ Untouched j.key st.log
  held : ∀ j ∈ st.held, ansOf st.store j.key = some .pendingReplay ∧ Untouched j.key st.log
  fresh : ∀ j ∈ all, j.key ∉ st.admitted.map (·.key) →
    ansOf st.store j.key = some .pending ∧ Untouched j.key st.log

theorem invW_init (all : List Job) : InvW all (WS.init (admitStore all)) := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp; simp [WS.init] at hp
  · intro j hj; simp [WS.init] at hj
  · intro j hj; simp [WS.init] at hj
  · intro j hj _
    obtain ⟨e, he, ha⟩ := lookup_admit hj
    refine ⟨show ansOf (admitStore all) j.key = _ by simp [ansOf, he, ha], ?_⟩
    intro t ht; simp [WS.init] at ht

/-! The moves, one at a time. -/

theorem takeHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job}
    (hI : InvW all st) (hh : ofAgent a st.held = h :: t) : InvW all (takeHeld a h st) := by
  refine ⟨hI.settled, ?_, ?_, hI.fresh⟩
  · intro j hj
    simp only [takeHeld, List.mem_append, List.mem_singleton] at hj
    rcases hj with (hj | hj) | hj
    · exact hI.live j (List.mem_append_left _ hj)
    · subst hj
      obtain ⟨h1, h2⟩ := hI.held _ (head_mem hh)
      exact ⟨⟨_, h1, rfl⟩, h2⟩
    · exact hI.live j (List.mem_append_right _ hj)
  · intro j hj; exact hI.held j (mem_dropFirst hh hj)

theorem takeQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job}
    (hI : InvW all st) (hq : ofAgent a st.queue = q :: t) : InvW all (takeQueued a q st) := by
  refine ⟨hI.settled, ?_, hI.held, hI.fresh⟩
  intro j hj
  simp only [takeQueued, List.mem_append, List.mem_singleton] at hj
  rcases hj with (hj | hj) | hj
  · exact hI.live j (List.mem_append_left _ hj)
  · subst hj; exact hI.live _ (List.mem_append_right _ (head_mem hq))
  · exact hI.live j (List.mem_append_right _ (mem_dropFirst hq hj))

theorem parkQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hq : ofAgent a st.queue = q :: t) :
    InvW all (parkQueued now a q st) := by
  have hqm := head_mem hq
  have hqA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hqm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hqm)]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hqm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append, List.mem_singleton] at hj
    rcases hj with hj | hj
    · obtain ⟨h1, h2⟩ := hI.held j hj
      exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hqm)]; exact h1, h2⟩
    · subst hj
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live _ (List.mem_append_right _ hqm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hqA)
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hh : ofAgent a st.held = h :: t) :
    InvW all (refuseHeld now a h st) := by
  have hm := head_mem hh
  have hA : h.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm h (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseHeld, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (hK.sh p hp h hm)]; exact h1, h2⟩
    · subst hp
      obtain ⟨h1, h2⟩ := hI.held h hm
      exact ⟨ansOf_settleOne_self _ now h1 rfl, Or.inr (Or.inr ⟨rfl, hf, h2⟩)⟩
  · intro j hj
    have hne : j.key ≠ h.key := by
      rcases List.mem_append.mp hj with hj | hj
      · exact hK.rh j hj h hm
      · exact fun e => hK.hq h hm j hj e.symm
    obtain ⟨h1, h2⟩ := hI.live j hj
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    have hj' : j ∈ dropFirst a st.held := hj
    obtain ⟨h1, h2⟩ := hI.held j (mem_dropFirst hh hj')
    exact ⟨by
      simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (key_ne_dropFirst hh hK.held hj')]; exact h1,
      h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ h.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hq : ofAgent a st.queue = q :: t) :
    InvW all (refuseQueued now a q st) := by
  have hm := head_mem hq
  have hA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseQueued, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hm)]; exact h1,
        h2⟩
    · subst hp
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live q (List.mem_append_right _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, Or.inr (Or.inr ⟨rfl, hf, h3⟩)⟩
  · intro j hj
    simp only [refuseQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    obtain ⟨h1, h2⟩ := hI.held j hj
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hm)]; exact h1, h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem settleRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (x : Answer) (b : Batch) (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t)
    (hxb : (x = .applied ∧ b = [.write j.key, .consume j.key .applied]) ∨
      (x = .failed ∧ b = [.consume j.key .failed])) :
    InvW all (settleRun now a j x b st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  have hb : ∀ m ∈ b, m = .write j.key ∨ ∃ t, m = .consume j.key t := by
    rcases hxb with ⟨_, rfl⟩ | ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      rcases hm' with rfl | rfl
      · exact Or.inl rfl
      · exact Or.inr ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      subst hm'; exact Or.inr ⟨_, rfl⟩
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [settleRun, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      have hne := hK.sr p hp j hm
      refine ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, ?_⟩
      simp only [settleRun]
      rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
      · exact Or.inl ⟨e1, List.mem_append_left _ e2⟩
      · exact Or.inr (Or.inl ⟨e1, List.mem_append_left _ e2⟩)
      · exact Or.inr (Or.inr ⟨e1, e2, untouched_snoc hne hb e3⟩)
    · subst hp
      obtain ⟨⟨y, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ hm)
      refine ⟨ansOf_settleOne_self _ now h1 h2, ?_⟩
      simp only [settleRun]
      rcases hxb with ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩
      · exact Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩
      · exact Or.inr (Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩)
  · intro y hy
    simp only [settleRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy
    have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
    obtain ⟨h1, h2⟩ := hI.held y hy
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩

theorem holdRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t) :
    InvW all (holdRun now a j st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ (hK.sr p hp j hm)]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_cons] at hy
    rcases hy with rfl | hy
    · obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live y (List.mem_append_left _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
    · have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
      obtain ⟨h1, h2⟩ := hI.held y hy
      exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem startW_inv (now : Int) (a : Nat) {all : List Job} {st : WS} (hI : InvW all st)
    (hK : KeysW st) : InvW all (startW now a st) := by
  unfold startW
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hI
  · rw [ite_eq_right hr]
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hI
      | cons q t =>
        simp only
        split
        · next hf => exact refuseQueued_inv now hI hK hf hq
        · exact takeQueued_inv hI hq
    | cons h t =>
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact hI
          · exact takeHeld_inv hI hh
      | cons q t' =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact parkQueued_inv now hI hK hq
          · exact takeHeld_inv hI hh

theorem finishW_inv (now : Int) (a : Nat) (r : RunResult) {all : List Job} {st : WS}
    (hI : InvW all st) (hK : KeysW st) : InvW all (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hI
  | cons j t =>
    simp only
    cases r with
    | ok => exact settleRun_inv now _ _ hI hK hj (Or.inl ⟨rfl, rfl⟩)
    | err e =>
      simp only
      split
      · exact holdRun_inv now hI hK hj
      · exact settleRun_inv now _ _ hI hK hj (Or.inr ⟨rfl, rfl⟩)

theorem stepW_admitted (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    ∃ l, (stepW c sz now st ev).admitted = st.admitted ++ l := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact ⟨[j], rfl⟩
    · split <;> exact ⟨[], by simp⟩
  | start a =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold startW
    split
    · rfl
    · split
      · split
        · simp [refuseHeld]
        · split
          · simp [parkQueued]
          · simp [takeHeld]
      · split
        · simp [refuseHeld]
        · split
          · rfl
          · simp [takeHeld]
      · split
        · simp [refuseQueued]
        · simp [takeQueued]
      · rfl
  | finish a r =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold finishW
    split
    · rfl
    · split
      · simp [settleRun]
      · split
        · simp [holdRun]
        · simp [settleRun]
  | wake a => exact ⟨[], by simp [stepW]⟩
  | fence => exact ⟨[], by simp [stepW]⟩

theorem runW_admitted (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), ∃ l, (runW c sz now st evs).admitted = st.admitted ++ l := by
  intro evs
  induction evs with
  | nil => intro st; exact ⟨[], by simp [runW]⟩
  | cons e es ih =>
    intro st
    obtain ⟨l1, h1⟩ := stepW_admitted c sz now st e
    obtain ⟨l2, h2⟩ := ih (stepW c sz now st e)
    exact ⟨l1 ++ l2, by simp only [runW]; rw [h2, h1, List.append_assoc]⟩

theorem stepW_inv (c : Caps) (sz : Job → Nat) (now : Int) {all : List Job} (st : WS) (ev : Ev)
    (hO : OrdW st) (hI : InvW all st)
    (hnd : ((stepW c sz now st ev).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (stepW c sz now st ev).admitted, j ∈ all) :
    InvW all (stepW c sz now st ev) := by
  obtain ⟨l, hl⟩ := stepW_admitted c sz now st ev
  have hnd0 : (st.admitted.map (·.key)).Nodup := by
    rw [hl, List.map_append] at hnd; exact (List.nodup_append.mp hnd).1
  have hK := keysW_of st hO hnd0
  cases ev with
  | submit j =>
    simp only [stepW] at hnd hall ⊢
    split
    · next hfit =>
      rw [ite_eq_left hfit] at hnd hall
      have hjn : j.key ∉ st.admitted.map (·.key) := by
        simp only [List.map_append, List.map_cons, List.map_nil] at hnd
        exact fun hm => (List.nodup_append.mp hnd).2.2 _ hm _ List.mem_cons_self rfl
      have hja : j ∈ all := hall j (List.mem_append_right _ List.mem_cons_self)
      refine ⟨hI.settled, ?_, hI.held, ?_⟩
      · intro y hy
        simp only [List.mem_append, List.mem_singleton] at hy
        rcases hy with hy | hy | hy
        · exact hI.live y (List.mem_append_left _ hy)
        · exact hI.live y (List.mem_append_right _ hy)
        · subst hy
          obtain ⟨h1, h2⟩ := hI.fresh y hja hjn
          exact ⟨⟨_, h1, rfl⟩, h2⟩
      · intro y hy hn
        exact hI.fresh y hy (fun hm => hn (by
          rw [List.map_append]; exact List.mem_append_left _ hm))
    · split
      · exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
      · exact hI
  | start a => exact startW_inv now a hI hK
  | finish a r => exact finishW_inv now a r hI hK
  | wake a => exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
  | fence =>
    refine ⟨?_, hI.live, hI.held, hI.fresh⟩
    intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    refine ⟨h1, ?_⟩
    rcases h2 with h | h | ⟨e1, _, e3⟩
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
    · exact Or.inr (Or.inr ⟨e1, rfl, e3⟩)

theorem runW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) :
    ∀ (evs : List Ev) (st : WS), OrdW st → InvW all st →
      ((runW c sz now st evs).admitted.map (·.key)).Nodup →
      (∀ j ∈ (runW c sz now st evs).admitted, j ∈ all) → InvW all (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st _ hI _ _; exact hI
  | cons e es ih =>
    intro st hO hI hnd hall
    obtain ⟨l, hl⟩ := runW_admitted c sz now es (stepW c sz now st e)
    have hnd' : (runW c sz now (stepW c sz now st e) es).admitted.map (·.key) =
        (stepW c sz now st e).admitted.map (·.key) ++ l.map (·.key) := by
      rw [hl, List.map_append]
    have hnd1 : ((stepW c sz now st e).admitted.map (·.key)).Nodup := by
      have := hnd; simp only [runW] at this; rw [hnd'] at this
      exact (List.nodup_append.mp this).1
    have hall1 : ∀ j ∈ (stepW c sz now st e).admitted, j ∈ all := by
      intro j hj; apply hall; simp only [runW]; rw [hl]; exact List.mem_append_left _ hj
    exact ih _ (stepW_ord c sz now st e hO) (stepW_inv c sz now st e hO hI hnd1 hall1) hnd hall

/-- The current workers' reachable states keep `InvW`. -/
theorem sessionW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev)
    (hnd : ((runW c sz now (WS.init (admitStore all)) evs).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (runW c sz now (WS.init (admitStore all)) evs).admitted, j ∈ all) :
    InvW all (runW c sz now (WS.init (admitStore all)) evs) :=
  runW_inv c sz now all evs _ (ordW_init _) (invW_init all) hnd hall

/-! ### The close -/

theorem ansOf_closeW_settled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {x : Answer}
    (h : ansOf st.store k = some x) (hs : x.isSettled = true) (hc : x ≠ .closeUnflushed) :
    ansOf (closeW flush now st) k = some x := by
  unfold ansOf closeW at *
  rw [lookup_settleDeferred]
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = x := by simpa using h
    split
    · simp [deferEntry_of_settled flush now e (he ▸ hs) (he ▸ hc), he]
    · simp [he]

/-- What the close answers for a write it set aside. -/
def closeAnswer : CloseFlush → Answer
  | .durable => .intentRecorded
  | .incomplete => .closeUnflushed

theorem ansOf_closeW_unsettled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {a : Answer}
    (h : ansOf st.store k = some a) (hu : a.isSettled = false) :
    k ∈ deferredW st ∧ ansOf (closeW flush now st) k = some (closeAnswer flush) := by
  unfold ansOf at h
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = a := by simpa using h
    have hu' : e.answer.isSettled = false := he ▸ hu
    have hm : k ∈ deferredW st := mem_pendingIds (lookup_mem _ _ _ hl) hu'
    refine ⟨hm, ?_⟩
    unfold ansOf closeW
    rw [lookup_settleDeferred, ite_eq_left hm, hl]
    have hs : ¬ e.answer.isSettled = true := by rw [hu']; decide
    cases flush with
    | durable =>
      have hc : e.answer ≠ .closeUnflushed := by
        intro hc; rw [hc] at hu'; exact absurd hu' (by decide)
      simp only [Option.map_some, deferEntry_durable, ite_eq_right hc,
        Lambo.Receipts.settle_unsettled e _ now hs]
      rfl
    | incomplete =>
      simp only [Option.map_some, deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now hs]
      rfl

theorem owedW_of {all : List Job} {st : WS} {j : Job} (hj : j ∈ all) (hu : Untouched j.key st.log) :
    OwedW all st j := by
  apply List.mem_filter.mpr
  refine ⟨(fixedLoad_perm _).mem_iff.mpr ?_, rfl⟩
  unfold intentsOf
  rw [List.flatten_append, List.foldl_append, ← intentsOf, intentsOf_admit]
  exact foldl_keeps_owed _ _ (List.mem_map.mpr ⟨j, hj, rfl⟩) hu

/-- **Shutdown accounting over the current worker.** After any session of the
current workers (admission, holds, retries, the fence) and a `close()`, every
admitted write is exactly one of: applied (its write and consume in one
batch); failed with its intent consumed `failed`; failed after the fence,
never run, its intent left owed for the session's current holder;
`intent_durable` after a completed final flush, owed in the flushed log (its
row is unconsumed, so the next serve's replay owes it), whether it was
running, held or queued when the close began; or `close_unflushed` after an
incomplete flush. None is left `pending` or `pending_replay`. The current
counterpart of `shutdown_no_silent_loss`. -/
theorem shutdown_no_silent_lossW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (now' : Int) (j : Job) (hj : j ∈ st.admitted) :
    ((j, Answer.applied) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .failed ∧
      [Mut.consume j.key .failed] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧
      ansOf (closeW flush now' st) j.key = some .intentRecorded ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .incomplete ∧
      ansOf (closeW flush now' st) j.key = some .closeUnflushed) := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hja := hall j hj
  have hD := mem_DW_of_admitted hO hj
  simp only [DW, List.mem_append] at hD
  rcases hD with ((hD | hD) | hD) | hD
  · obtain ⟨p, hp, hpj⟩ := List.mem_map.mp hD
    subst hpj
    obtain ⟨h1, h2⟩ := hI.settled p hp
    rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
    · refine Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩)
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inr (Or.inl ⟨by rw [← e1]; exact hp, e2, ?_, owedW_of hja e3⟩))
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨h1, h3⟩ := hI.held j hD
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 rfl).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_right _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))


/-- **A held write at close.** A write held in session (an outage, a run of
lookup timeouts, or behind an earlier held write of its agent) answers
`pending_replay`, never `failed`, and its intent is owed while the session
runs; the close answers it `intent_durable` after a completed final flush (and
it stays owed for the next serve's replay) and `close_unflushed` otherwise. -/
theorem held_at_closeW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev}
    {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (j : Job) (hj : j ∈ st.held) (flush : CloseFlush) (now' : Int) :
    ansOf st.store j.key = some .pendingReplay ∧ OwedW all st j ∧
      ansOf (closeW flush now' st) j.key = some (closeAnswer flush) := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨h1, h2⟩ := hI.held j hj
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hj]))
  exact ⟨h1, owedW_of hja h2, (ansOf_closeW_unsettled h1 rfl).2⟩

/-- **An in-session outage holds the write.** The running write of agent `a`
that meets an embedder outage becomes a held write answering `pending_replay`,
appends nothing (its intent is not consumed), and stays owed. While it stays
held, `held_at_closeW` says the same of every later state and of the close.
The current counterpart of `outage_failed_but_owed`, which states the worker
before #295, where the same write answered `failed`. Rust test
`a_write_reached_during_an_in_session_embedder_outage_is_not_consumed`. -/
theorem outage_holdsW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev}
    {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t) :
    j ∈ (stepW c sz now st (.finish a (.err .unavailable))).held ∧
      ansOf (stepW c sz now st (.finish a (.err .unavailable))).store j.key = some .pendingReplay ∧
      (stepW c sz now st (.finish a (.err .unavailable))).log = st.log ∧
      OwedW all (stepW c sz now st (.finish a (.err .unavailable))) j := by
  have hfin : stepW c sz now st (.finish a (.err .unavailable)) = holdRun now a j st := by
    simp [stepW, finishW, hrun, holdsS]
  rw [hfin]
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hm := head_mem hrun
  obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hm)
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hm]))
  exact ⟨List.mem_cons_self, ansOf_settleOne_self _ now h1 h2, rfl, owedW_of hja h3⟩

/-- The error the model hands `finish` for a run that failed with `e`: the
worker's hold decision in `spawn_worker`, before its counters move. A lookup
timeout on a write's first attempt adds one to `lookup_streak` (lookup
timeouts in a row, across lanes, with nothing applied between; `streak` is its
value before), and the one that brings it to `REPLAY_LOOKUP_TIMEOUT_LIMIT`
holds the write as an outage does; a retried held write's lookup timeout does
not count and settles. This is the abstraction `stepW`'s header describes
("handed to the model as `unavailable`"), now as a function. -/
def heldErrW (retried : Bool) (streak : Nat) : Err → Err
  | .lookupTimeout =>
    if retried = false ∧ lookupTimeoutLimit ≤ streak + 1 then .unavailable else .lookupTimeout
  | e => e

/-- The worker holds exactly on an outage or on the lookup timeout that ends a
run of `REPLAY_LOOKUP_TIMEOUT_LIMIT` first attempts. -/
theorem heldErrW_holds (retried : Bool) (streak : Nat) (e : Err) :
    holdsS (heldErrW retried streak e) = true ↔
      (e = .unavailable ∨ (e = .lookupTimeout ∧ retried = false ∧ lookupTimeoutLimit ≤ streak + 1)) := by
  cases e <;> simp only [heldErrW] <;> (try split) <;> simp_all [holdsS] <;> decide

/-- **A single lookup timeout settles `failed` and consumes the intent**
(below the run limit, or the retry of a held write): the write is settled
`failed` and its batch is the consume. Rust test
`a_lookup_timeout_in_session_settles_failed_and_holds_nothing`. -/
theorem lookupTimeout_settlesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t)
    (retried : Bool) (streak : Nat) (hone : retried = true ∨ streak + 1 < lookupTimeoutLimit) :
    let st' := stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout)))
    ansOf st'.store j.key = some .failed ∧ (j, Answer.failed) ∈ st'.settled ∧
      st'.log = st.log ++ [[Mut.consume j.key .failed]] := by
  have he : heldErrW retried streak .lookupTimeout = .lookupTimeout := by
    simp only [heldErrW]
    rw [ite_eq_right]
    rintro ⟨h1, h2⟩
    rcases hone with h | h
    · rw [h] at h1; cases h1
    · omega
  have hfin : stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout))) =
      settleRun now a j .failed [.consume j.key .failed] st := by
    rw [he]; simp [stepW, finishW, hrun, holdsS]
  simp only [hfin]
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨⟨x, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ (head_mem hrun))
  exact ⟨ansOf_settleOne_self _ now h1 h2, List.mem_append_right _ (List.mem_singleton.mpr rfl), rfl⟩

/-- **A run of lookup timeouts holds the write** that ends it, as an outage
does (`outage_holdsW`). Rust test
`a_lookup_timeout_run_holds_the_third_write_and_its_retry_settles_it`. -/
theorem lookupTimeout_run_holdsW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t)
    (streak : Nat) (hrun3 : lookupTimeoutLimit ≤ streak + 1) :
    let st' := stepW c sz now st (.finish a (.err (heldErrW false streak .lookupTimeout)))
    j ∈ st'.held ∧ ansOf st'.store j.key = some .pendingReplay ∧ st'.log = st.log ∧
      OwedW all st' j := by
  have he : heldErrW false streak .lookupTimeout = .unavailable := by
    simp [heldErrW, hrun3]
  simp only [he]
  exact outage_holdsW hst hnd hall a j t hrun

/-- **A retried close upgrades `close_unflushed`.** A write the close set aside
(running, held or queued) answers `close_unflushed` after an incomplete flush;
a retried close whose flush completes settles the same set-aside ids
(`close_deferred`, kept after an incomplete flush) and answers it
`intent_durable`. `retry_upgrades` is the same fact for one entry. -/
theorem closeW_retry_upgradesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (j : Job) (hj : j ∈ st.running ++ st.held ++ st.queue) (now1 now2 : Int) :
    ansOf (closeW .incomplete now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferred .durable now2 (deferredW st) (closeW .incomplete now1 st)) j.key =
        some .intentRecorded := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  have hu : ∃ a, ansOf (runW c sz now (WS.init (admitStore all)) evs).store j.key = some a ∧
      a.isSettled = false := by
    simp only [List.mem_append] at hj
    rcases hj with (hj | hj) | hj
    · exact (hI.live j (List.mem_append_left _ hj)).1
    · exact ⟨_, (hI.held j hj).1, rfl⟩
    · exact (hI.live j (List.mem_append_right _ hj)).1
  obtain ⟨a, h1, h2⟩ := hu
  obtain ⟨hm, hc⟩ := ansOf_closeW_unsettled (flush := .incomplete) (now := now1) h1 h2
  refine ⟨hc, ?_⟩
  unfold ansOf at h1 ⊢
  cases hl : lookup (runW c sz now (WS.init (admitStore all)) evs).store j.key with
  | none => rw [hl] at h1; simp at h1
  | some e =>
    rw [hl] at h1
    have he : e.answer.isSettled = false := by
      have : e.answer = a := by simpa using h1
      rw [this]; exact h2
    unfold closeW
    rw [lookup_settleDeferred, ite_eq_left hm, lookup_settleDeferred, ite_eq_left hm, hl]
    simp only [Option.map_some]
    rw [retry_upgrades now1 now2 e he]

/-! ### Crash and the serve that follows -/

theorem startW_log (now : Int) (a : Nat) (st : WS) : (startW now a st).log = st.log := by
  unfold startW
  split
  · rfl
  · split
    · split
      · rfl
      · split <;> rfl
    · split
      · rfl
      · split <;> rfl
    · split <;> rfl
    · rfl

theorem stepW_log (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    (stepW c sz now st ev).log = st.log ∨
      ∃ j r, (stepW c sz now st ev).log = st.log ++ [(workerStep false j r).2] := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact Or.inl rfl
    · split <;> exact Or.inl rfl
  | start a => exact Or.inl (startW_log now a st)
  | finish a r =>
    simp only [stepW]
    unfold finishW
    split
    · exact Or.inl rfl
    · next j _ _ =>
      cases r with
      | ok => exact Or.inr ⟨j, .ok, rfl⟩
      | err e =>
        simp only
        split
        · exact Or.inl rfl
        · exact Or.inr ⟨j, .err .embed, rfl⟩
  | wake a => exact Or.inl rfl
  | fence => exact Or.inl rfl

/-- **The refinement.** Every batch the current workers append is a batch the
old worker (`workerStep`, unfenced) appends for some write and run result: an
applied write's is `workerStep`'s success batch, a consumed failure's is its
`[consume failed]` (for a lookup timeout the old worker appended nothing; the
batch is the one it appends for a content refusal), and a hold or a fenced
refusal appends none. So `workerStep_atomic` and `workerStep_keys` hold of the
current worker's batches, which is what the crash argument needs. -/
theorem runW_log_refines (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), (∀ b ∈ st.log, ∃ j r, b = (workerStep false j r).2) →
      ∀ b ∈ (runW c sz now st evs).log, ∃ j r, b = (workerStep false j r).2 := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih =>
    intro st h
    apply ih
    intro b hb
    rcases stepW_log c sz now st e with hl | ⟨j, r, hl⟩
    · rw [hl] at hb; exact h b hb
    · rw [hl] at hb
      cases List.mem_append.mp hb with
      | inl hb => exact h b hb
      | inr hb => exact ⟨j, r, List.mem_singleton.mp hb⟩

/-- A row still unconsumed after a run of consumes was there, unconsumed,
before it. -/
theorem foldl_pending_mem {i' : Intent} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    i' ∈ ms.foldl applyMut acc → i'.outcome = none → i' ∈ acc := by
  intro ms
  induction ms with
  | nil => intro acc _ h _; exact h
  | cons m ms ih =>
    intro acc hnp h ho
    have h1 := ih (applyMut acc m) (fun x hx => hnp x (List.mem_cons_of_mem _ hx)) h ho
    cases m with
    | put k => exact absurd rfl (hnp _ List.mem_cons_self k)
    | write _ => exact h1
    | consume q t =>
      obtain ⟨i0, h0, he⟩ := List.mem_map.mp h1
      by_cases hq : i0.job.key = q
      · rw [ite_eq_left hq] at he; rw [← he] at ho; simp at ho
      · rw [ite_eq_right hq] at he; rw [← he]; exact h0

/-- **The current serve applies only stored, unconsumed rows.** Every write
`serveR` (both passes, the block reason after a halt) applies is a row it was
given, unconsumed: the first pass by `replayD_facts`, the retry pass by
`loopD_facts` over the rows the first pass's consumes left unconsumed
(`foldl_pending_mem`). The current counterpart of `replay_applied_pending`. -/
theorem serveR_applied_pending {live : Bool} {rout rout' : Job → RR} {s' : Streaks}
    {intents : List Intent} {x : Job} (hx : x ∈ (serveR live rout rout' s' intents).applied) :
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none := by
  rw [(serveR_log live rout rout' s' intents).2, (serveQ_log live rout rout' s' intents).2] at hx
  unfold replayP at hx
  dsimp only at hx
  split at hx
  · exact replayD_applied_pending hx
  · rcases List.mem_append.mp hx with h | h
    · exact replayD_applied_pending h
    · have hs := (loopD_facts stepQ rout' _ s' []).1.subset h
      obtain ⟨i', hi', rfl⟩ := List.mem_map.mp hs
      obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi'
      have hio : i'.outcome = none := Option.isNone_iff_eq_none.mp hi2
      have hnp := (replayD_facts stepQ live rout intents).2.2.1
      exact ⟨i', foldl_pending_mem _ _ hnp hi1 hio, rfl, hio⟩

/-- **No write is applied twice across a crash, over the current worker and
serve.** A flush makes some prefix of the session's log durable (admission's
puts, then the current workers' batches), and the process may die after any
flush. If a write is in the durable prefix, its consume is in the same batch
(`runW_log_refines`, `workerStep_atomic`), so the next serve (`serveR` over
the current load order, both passes) never applies it again. The current
counterpart of `crash_never_reapplies`; like it, about one process's log over
an empty intent table. -/
theorem crash_never_reappliesW (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore)
    (evs : List Ev) (all : List Job) (n : Nat) (r : Key) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks)
    (hw : Mut.write r ∈ ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten) :
    ∀ x ∈ (serveR live rout rout' s' (fixedLoad (intentsOf
      ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten))).applied,
      x.key ≠ r := by
  intro x hx hxr
  obtain ⟨i, hi, hix, hio⟩ := serveR_applied_pending hx
  rw [(fixedLoad_perm _).mem_iff] at hi
  have hL := runW_log_refines c sz now evs (WS.init s) (by intro b hb; simp [WS.init] at hb)
  generalize (runW c sz now (WS.init s) evs).log = L at hw hi hL
  rw [List.take_append, List.flatten_append] at hw hi
  have hwB : Mut.write r ∈ (L.take (n - (admitLog all).length)).flatten := by
    cases List.mem_append.mp hw with
    | inl h =>
      obtain ⟨j, hj⟩ := mem_admit_flatten h
      cases hj
    | inr h => exact h
  have hcB : Mut.consume r .applied ∈ (L.take (n - (admitLog all).length)).flatten := by
    obtain ⟨b, hb, hwb⟩ := List.mem_flatten.mp hwB
    obtain ⟨j, rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    exact List.mem_flatten.mpr ⟨_, hb, workerStep_atomic hwb⟩
  have hnp : NoPut (L.take (n - (admitLog all).length)).flatten := by
    intro m hm j he
    obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
    obtain ⟨j', rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he
  unfold intentsOf at hi
  rw [List.foldl_append] at hi
  exact foldl_consumed _ _ hnp (Or.inl ⟨_, hcB⟩) i hi (by rw [hix]; exact hxr) hio

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

/-! The next four examples are over the worker **before #295** (`work`,
`close`); the current worker's are under "The close over the current worker"
at the end of this section. -/

/-- `drain_settles_each` (before #295): one job applied, one refused for its
content, one hit by an outage; all three settled. The current worker holds the
third (`pending_replay`) instead. -/
example :
    let w := work (fun _ => false)
      (fun j => if j.rid = 1 then .ok else if j.rid = 2 then .err .embed else .err .unavailable)
      0 (admitStore [j1, j2, j3]) [j1, j2, j3]
    ([j1.key, j2.key, j3.key].map (fun r => (lookup w.1 r).map (·.answer)) =
      [some .applied, some .failed, some .failed]) ∧
    w.2 = [[.write j1.key, .consume j1.key .applied], [.consume j2.key .failed], []] := by decide

/-- `shutdown_no_silent_loss` (before #295): the drain finished `j1`; `j2` and
`j3` are deferred and owed by the next replay. -/
example :
    let c := close [j1] (fun _ => false) (fun _ => .ok) 0 .durable (admitStore [j1, j2, j3])
    [j1.key, j2.key, j3.key].map (fun r => (lookup c.1 r).map (·.answer)) =
        [some .applied, some .intentRecorded, some .intentRecorded] ∧
      (pendingOf (loadOrder (intentsOf (admitLog [j1, j2, j3] ++ c.2).flatten))).map (·.job) =
        [j2, j3] := by decide

/-- `close_answer` (before #295), incomplete flush: the deferred write says
`close_unflushed`, and a retried close whose flush lands upgrades it
(`retry_upgrades`). -/
example :
    let c := close [] (fun _ => false) (fun _ => .ok) 0 .incomplete (admitStore [j1])
    (lookup c.1 j1.key).map (·.answer) = some .closeUnflushed ∧
      (lookup (settleDeferred .durable 1 [j1.key] c.1) j1.key).map (·.answer) = some .intentRecorded := by
  decide

/-- `drop_settles_nothing`: the `Drop` path leaves a pending receipt pending. -/
example : (lookup (abortAllSync (admitStore [j1])) j1.key).map (·.answer) = some .pending := by decide

/-- `crash_never_reapplies` (before #295): the process dies after flushing
`j1`'s commit; the next replay applies only `j2`. -/
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
`j1`. Over the worker **before #295** (`close`); the current worker holds `j2`
behind `j1` (`stepW_ord`, `sessionW_then_serve_order`, `outage_holdsW`). -/
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
or "yet".

**This finding is about the worker before #295.** #295 removed its cause:
the current worker no longer answers `failed` for an outage but holds the
write (`pending_replay`, `outage_holdsW`), and the close answers a held write
`intent_durable` (`held_at_closeW`), so the receipt goes from
`pending_replay` or `intent_durable` to `applied_after_restart` with no
`failed` between. The `ReceiptAnswer::Failed` rustdoc and mcp.mdx's `failed`
row now say an outage does not answer `failed`. -/

/-- The restart map's answer for a receipt after a replay: seeded from the
rows, then overwritten by the replay's own inserts. -/
def restartAnswer (live : Bool) (rout : Job → RR) (intents : List Intent) (r : Key) :
    Option Answer :=
  lookupLast (seed intents ++ (replay live rout intents).answers) r

/-- **Before #295: a `failed` receipt for an outage's write later answered
`applied_after_restart`.** Under the worker before #295 (`close`, `workerStep`)
a write met by an in-session outage answers `failed`, and the next serve
applies it. **The current Rust does not behave this way**: since #295 that
write answers `pending_replay` in session and `intent_durable` at close, never
`failed` (`outage_holdsW`, `held_at_closeW`; the example "`outage_holdsW`
beside `outage_failed_then_applied`" at the end of this section runs both on
this scenario). Kept because it documents the receipt #295's hold replaced,
the finding above. -/
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

/-! ### The fixes for #295 on the issue's scenarios -/

private def k1 : Job := ⟨3, 1, 1002, false, 0⟩

/-- `transient_skip_reorders` fixed: `j1` meets one transient answer, so
agent 0 is deferred and its `j2` waits; agent 1's `k1` applies. The next
serve applies `j1`, then `j2`. The old replay applied `j2` first. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r1 := replayD stepFixed true rout ints
    let r2 := replayD stepFixed true okAll (applyLog ints r1.log)
    (replay true rout ints).applied = [j2, k1] ∧
      r1.applied = [k1] ∧ r1.block = none ∧ r2.applied = [j1, j2] ∧
      r1.applied ++ r2.applied = [k1, j1, j2] := by
  decide

/-- The threshold still counts transients across agents: three agents each
meeting one, with nothing answered between, stop the replay as `embedder`,
consuming nothing. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 1, 1001, false, 0⟩
    let c : Job := ⟨3, 2, 1002, false, 0⟩
    let r := replayD stepFixed true (fun _ => .run (.err .unavailable))
      [⟨a, none⟩, ⟨b, none⟩, ⟨c, none⟩]
    r.log = [] ∧ r.block = some .embedder := by
  decide

/-- `clock_step_reorders` fixed: the clock stepped back between `a` and `b`
of one process; the fixed load puts `a` (sequence 1) first anyway, and the
replay applies `a`, then `b`. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 0, 999, false, 0⟩
    fixedLoad [⟨a, none⟩, ⟨b, none⟩] = [⟨a, none⟩, ⟨b, none⟩] ∧
      (replayD stepFixed true okAll (fixedLoad [⟨a, none⟩, ⟨b, none⟩])).applied = [a, b] := by
  decide

/-- Across processes the order is the old one: two crashed processes whose
clocks interleave keep their positions, and within process 7 the clock step
is undone. -/
example :
    let a1 : Job := ⟨1, 0, 1000, false, 7⟩
    let a2 : Job := ⟨2, 0, 990, false, 7⟩
    let b1 : Job := ⟨1, 1, 995, false, 9⟩
    let rows : List Intent := [⟨a1, none⟩, ⟨a2, none⟩, ⟨b1, none⟩]
    (loadOrder rows).map (·.job) = [a2, b1, a1] ∧
      (fixedLoad rows).map (·.job) = [a1, b1, a2] := by
  decide

/-- `bad_id_never_settled` fixed: the row with the unparseable id is settled
`failed`, and nothing is left owed. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .badId else .run .ok
    let r := replayD stepFixed true rout ints
    r.applied = [j2] ∧ r.failed = [j1] ∧ pendingOf (applyLog ints r.log) = [] := by
  decide

/-- #294's scenario through the fully fixed replay. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .run (.err .inputRefusal) else .run .ok
    let r := replayD stepFixed true rout ints
    r.applied = [j1, j3] ∧ r.failed = [j2] ∧ r.block = none := by
  decide

/-! ### The in-session hold on the issue's scenario -/

/-- `outage_then_replay_reorders` fixed. `j1` meets an outage; when `j2`
arrives the embedder is still down, so `j1` is retried, fails again, and
`j2` is held behind it, unrun. Both receipts say `pending_replay`; agent 1's
`k1` applies meanwhile. The next serve applies `j1`, then `j2`. -/
example :
    let out : Nat → Job → RunResult := fun _ j =>
      if j.agent = 0 then .err .unavailable else .ok
    let st := workH out 0 0 (HState.init (admitStore [j1, j2, k1])) [j1, j2, k1]
    let rows : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, some .applied⟩]
    st.applied = [k1] ∧ st.held = [j1, j2] ∧
      [j1.key, j2.key, k1.key].map (fun r => (lookup st.store r).map (·.answer)) =
        [some .pendingReplay, some .pendingReplay, some .applied] ∧
      (replayD stepFixed true okAll (fixedLoad rows)).applied = [j1, j2] := by
  decide

/-- The embedder is back when `j2` arrives: the held `j1` applies first, then
`j2`, in this serve. -/
example :
    let out : Nat → Job → RunResult := fun t _ => if t = 0 then .err .unavailable else .ok
    let st := workH out 0 0 (HState.init (admitStore [j1, j2])) [j1, j2]
    st.applied = [j1, j2] ∧ st.held = [] ∧
      [j1.key, j2.key].map (fun r => (lookup st.store r).map (·.answer)) =
        [some .applied, some .applied] := by
  decide

/-! ### The retry pass, quarantine and the block reason -/

/-- A write that blinked once no longer waits for the next serve: the retry
pass applies `j1`, then `j2`, after `k1`; nothing is owed and nothing is
blocked. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r := serveQ true rout okAll ⟨1, 0⟩ ints
    r.applied = [k1, j1, j2] ∧ r.block = none ∧ pendingOf (applyLog ints r.log) = [] := by
  decide

/-- A write that never embeds keeps its agent's writes owed, and the serve
says why (`embedder`). -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r := serveQ true rout rout ⟨1, 0⟩ ints
    r.applied = [k1] ∧ r.block = some .embedder ∧
      (pendingOf (applyLog ints r.log)).map (·.job) = [j1, j2] := by
  decide

/-- An id in another format is quarantined, not settled: it and its agent's
later write stay owed, `k1` applies, and the serve says `other`. The fix as
first committed settled it `failed`. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .unknownFormat else .run .ok
    let r := serveQ true rout rout ⟨0, 0⟩ ints
    (replayD stepFixed true rout ints).failed = [j1] ∧
      r.applied = [k1] ∧ r.failed = [] ∧ r.block = some .other ∧
      (pendingOf (applyLog ints r.log)).map (·.job) = [j1, j2] := by
  decide

/-! ### The hold with admission, caps, retries and the fence -/

private def capsT : Caps := ⟨2, 4, 100⟩
private def one : Job → Nat := fun _ => 1

/-- The lane wedge the review found, gone. `j1` meets an outage and is held;
`j2` is admitted and joins it; `j3` is refused, the lane of two being full of
held writes, and the refusal wakes the retry. With no further write, the
retry applies `j1`, then `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2, j3]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0, .submit j3]
    let st2 := runW capsT one 0 st [.wake 0, .start 0, .finish 0 .ok, .start 0, .finish 0 .ok]
    st.held = [j1, j2] ∧ st.admitted = [j1, j2] ∧ st2.applied = [j1, j2] ∧ st2.held = [] := by
  decide

/-- Held bytes count: with a 3-byte cap and 2-byte writes, the held `j1`
leaves no room for `j2`. -/
example :
    let st := runW ⟨8, 8, 3⟩ (fun _ => 2) 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2]
    st.held = [j1] ∧ st.admitted = [j1] := by
  decide

/-- The lease is lost in the middle of a retry: `j1`, already running,
commits (the store refuses that flush); the held `j2` is refused without
being run. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0,
       .wake 0, .start 0, .fence, .finish 0 .ok, .start 0]
    st.applied = [j1] ∧ st.settled.map (·.1) = [j1, j2] ∧ st.held = [] := by
  decide

/-- A lookup timeout settles `failed` and holds nothing: the agent's next
write applies. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .lookupTimeout), .submit j2, .start 0, .finish 0 .ok]
    st.held = [] ∧ st.settled = [(j1, .failed), (j2, .applied)] := by
  decide

/-- The halt the review found hidden: a replay halted on a lost lease leaves
`j2` owed and reports `null`; `serveR` reports `other`. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .halt else .run .ok
    reported (serveQ true rout rout ⟨0, 0⟩ ints).block = none ∧
      pendingOf (applyLog ints (serveQ true rout rout ⟨0, 0⟩ ints).log) ≠ [] ∧
      (serveR true rout rout ⟨0, 0⟩ ints).block = some .other := by
  decide

/-! ### The close over the current worker -/

/-- `held_at_closeW`, `shutdown_no_silent_lossW` on #295's scenario. `j1`
meets an outage and is held; `j2` joins it, unrun; agent 1's `k1` applies.
Both held writes answer `pending_replay` in session, never `failed`; the close
answers them `intent_durable` after a completed flush and `close_unflushed`
after an incomplete one, leaves `k1` `applied`, and the flushed log owes
exactly `j1` and `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2, k1]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0,
       .submit k1, .start 1, .finish 1 .ok]
    st.held = [j1, j2] ∧ st.applied = [k1] ∧
      [j1.key, j2.key, k1.key].map (ansOf st.store) =
        [some .pendingReplay, some .pendingReplay, some .applied] ∧
      [j1.key, j2.key, k1.key].map (ansOf (closeW .durable 1 st)) =
        [some .intentRecorded, some .intentRecorded, some .applied] ∧
      [j1.key, j2.key, k1.key].map (ansOf (closeW .incomplete 1 st)) =
        [some .closeUnflushed, some .closeUnflushed, some .applied] ∧
      (pendingOf (fixedLoad (intentsOf (admitLog [j1, j2, k1] ++ st.log).flatten))).map (·.job) =
        [j1, j2] := by
  decide

/-- `outage_holdsW` beside `outage_failed_then_applied`: the same write under
the current worker answers `intent_durable` at close (and the next serve
applies it), where the worker before #295 answered it `failed`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable)]
    let rows := fixedLoad (intentsOf (admitLog [j1] ++ st.log).flatten)
    ansOf (closeW .durable 1 st) j1.key = some .intentRecorded ∧
      (serveR true okAll okAll ⟨0, 0⟩ rows).applied = [j1] ∧
      (lookup (close [j1] (fun _ => false) (fun _ => .err .unavailable) 0 .durable
        (admitStore [j1])).1 j1.key).map (·.answer) = some .failed := by
  decide

/-- The fence at close: `j1`, already running, commits (its batch is in the
log; the store refuses that flush); `j2` is refused `failed` unrun and its
intent is left owed for the session's current holder. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0, .fence, .finish 0 .ok, .start 0]
    [j1.key, j2.key].map (ansOf (closeW .incomplete 1 st)) = [some .applied, some .failed] ∧
      st.log = [[.write j1.key, .consume j1.key .applied]] ∧
      (pendingOf (fixedLoad (intentsOf (admitLog [j1, j2] ++ st.log).flatten))).map (·.job) =
        [j2] := by
  decide

/-- A write running when the close starts (aborted before its commit) and one
still queued: both `close_unflushed` after an incomplete flush, and both
`intent_durable` once a retried close's flush lands (`closeW_retry_upgradesW`). -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0]
    st.running = [j1] ∧ st.queue = [j2] ∧
      [j1.key, j2.key].map (ansOf (closeW .incomplete 1 st)) =
        [some .closeUnflushed, some .closeUnflushed] ∧
      [j1.key, j2.key].map
        (ansOf (settleDeferred .durable 2 (deferredW st) (closeW .incomplete 1 st))) =
        [some .intentRecorded, some .intentRecorded] := by
  decide

/-- `crash_never_reappliesW`: the process dies after flushing `j1`'s commit;
the next serve applies only `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0, .finish 0 .ok, .start 0, .finish 0 .ok]
    let log := admitLog [j1, j2] ++ st.log
    (serveR true okAll okAll ⟨0, 0⟩ (fixedLoad (intentsOf (log.take 3).flatten))).applied =
      [j2] := by
  decide

end Examples

end Lambo.Drain
