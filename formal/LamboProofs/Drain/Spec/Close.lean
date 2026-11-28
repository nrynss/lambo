/-
# Write-queue drain: the close, crash and shutdown over the current worker

The close (`closeW`, `closeD`) after any trace of `Worker.lean`'s current
worker, a crash and the serve that follows it, the dead-lettered batches at
close (#345), worker panics and the dead-lettered set, and where the
lookup-timeout streak resets (#346).

This is the specification layer: the definitions and each result as a
proposition `Spec.<name>` (`Spec/Base.lean` describes the two layers). The
proofs are in `Drain/Proofs/Close.lean`.

## Results

* The close, crash and shutdown over the current worker (`closeW` after any
  `runW` trace; `InvW`, `sessionW_inv`): `shutdown_no_silent_lossW`,
  `shutdown_exactly_oneW`: after a close every admitted write is exactly one
  of applied, failed with its intent consumed, failed after the fence with its
  intent left unconsumed, `intent_durable` and owed in the flushed log, or
  `close_unflushed`, whether it was running, held or queued when the close
  began; `held_at_closeW`: a held write answers `pending_replay` while held,
  its intent unconsumed, and the close answers it `intent_durable` or
  `close_unflushed`, never `failed` (a held write's retry can still settle
  `failed` in session, on a content refusal or after the fence);
  `outage_holdsW`: an outage holds the write (the current counterpart of
  `outage_failed_but_owed`); `lookupTimeout_settlesW`,
  `lookupTimeout_run_holdsW`, `heldErrW_holds`: a lookup timeout settles
  `failed` and consumes the intent, except on a first attempt once the run of
  them reaches the limit, which holds; `closeW_retry_upgradesW`: a retried
  close upgrades `close_unflushed`; `runW_log_refines`: every batch of the
  current worker is one `workerStep` appends; `serveR_applied_pending`: the
  current serve applies only stored, unconsumed rows;
  `crash_never_reappliesW`: no write in a durable prefix of the current
  worker's log is applied again by the current serve.
* Dead-lettered batches at close (#345; `closeD` over a flush history,
  `Landed`): before the fix the close vouched for a put a dead letter dropped
  (`closeW_vouches_dead_letteredW`; the concrete case,
  `closeW_vouches_dead_lettered`, is in `Examples.lean`); at a close whose
  final flush completed, the current close answers a set-aside write
  `intent_durable` exactly when its put is committed (`closeD_durable_iff`; a
  close whose flush did not complete answers every set-aside write
  `close_unflushed`, committed put or not), never for a dropped one
  (`closeD_never_vouches_dropped`, `closeD_vouches_only_committed`), with the
  accounting `shutdown_no_silent_lossD`, and a retried close upgrades only a
  committed one (`closeD_retryD`). With no dead letter it is `closeW`
  (`closeD_agrees`), so the `closeW` theorems describe the current close of
  every session no flush dead-lettered.
* Worker panics and the dead-lettered set (#345 re-review; `stepK`,
  `stepP`): a lane worker that panics after its commit leaves its write
  unsettled for the close to set aside, which `stepW` does not model.
  Forgetting a dead-lettered receipt at the consume then vouches for a
  dropped put (`keptAtConsume_vouches_after_panic`); the current Rust
  forgets it at the settle, which never does
  (`settlePrune_never_vouches_dropped`, `settlePrune_exact`,
  `settlePrune_unsettled_bounded`, `settlePrune_settled_forgotten`).
* Where the lookup streak resets (#346; `streakOld`, `streakNew`,
  `runSince`): before the fix a timeout between a commit and that write's
  settle was counted toward the old run (`streak_old_counts_across_commit`),
  and with the reset in the worker's `Ok` arm, one between the commit and
  that arm (`streak_old_counts_across_commit`'s twin,
  `streak_okArm_counts_across_commit`); the current worker resets inside the
  commit's critical section, so the streak is the documented count
  (`streakNew_eq_runSince`, `heldNew_iff`) and a timeout after an applied
  write settles `failed` (`streakNew_after_commit`,
  `timeout_after_commit_settlesW`); both resets would forget a run
  (`streak_both_forgets_run`).

The final durability verdict also depends on the flush-task join.
`Flush.close_after_flush_panic_cannot_vouch` covers the persistent refusal
that prevents this close from choosing the durable receipt transition.

Boundary: each step is atomic, as in `Base.lean`; which puts the final flush
committed is an input (`Landed`), the flush loop itself being `Flush/`; the
lease fence at the flush is not modelled.
-/

import LamboProofs.Drain.Spec.Worker

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The close, crash and shutdown over the current worker (#295 on)

The close, crash and shutdown theorems in `History.lean` (`close`,
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
the session is degraded, or when the lease was lost. Since #345
`settle_deferred` also answers `close_unflushed` for a write whose put a
background flush dead-lettered, whatever it is passed (`closeD`, in the
section "Dead-lettered batches at close (#345)" below).

What the close does to an admitted write, by its state when `close()` starts:

* **applied** (`settleRun` on `.ok`): settled `applied`, its write and its
  consume in one batch; `settle_deferred` leaves it alone.
* **failed and consumed** (`settleRun` on an error that does not hold it: a
  content or input refusal, a lookup timeout below the run limit, or the
  retry of a write held by a run of them): settled `failed`, intent consumed
  `failed`; left alone.
* **fenced** (`refuseHeld`, `refuseQueued`: taken after the lease was lost):
  settled `failed` without being run, its intent left unconsumed (the
  session's current holder's to apply, if an earlier flush took it); left
  alone. In the Rust a fenced close refuses to flush (`close_fenced` passes
  `CloseFlush::Incomplete`, `src/memory/shutdown.rs`), so everything it sets
  aside answers `close_unflushed`. That is code-backed, not proved: the model
  takes the flush outcome as a free parameter, and `closeW .durable` over a
  fenced session answers `intent_durable`, which the Rust never does.
* **held** (`holdRun`, `parkQueued`: an embedder outage; a lookup timeout on a
  first attempt once `lookup_streak`, the lookup timeouts in a row with nothing
  applied between, has reached `REPLAY_LOOKUP_TIMEOUT_LIMIT`, which holds the
  third and every later one until a write applies; or a later write of a held
  agent): `pending_replay`, unsettled, intent unconsumed. `abort_workers`
  clears `Lanes::held` and sets the receipt aside; `settle_deferred` answers
  `intent_durable` after a completed final flush, `close_unflushed` otherwise.
  It answers `pending_replay` while held, and the close never answers it
  `failed`; a held write's retry can still settle `failed` in session (a
  content refusal, or a refusal after the fence).
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

**The drain trace over-approximates the sealed lanes.** The Rust seals the
lanes at the start of `quiesce` (`self.seal()`, `src/writeq/drain.rs`), and
from then on a held write's retry starts only on a lost lease, never because
it came due (`retry_due && !l.sealed`, `spawn_worker`). `stepW` has no seal
state, so a `runW` trace standing for the drain can `wake`, `start` and
`finish` a held write after the seal, which the Rust cannot. The model allows
more traces than the Rust, never fewer, so every theorem over all `runW`
traces holds of the Rust's. A retry that started before the seal and finishes
during the drain is one of the Rust's traces too.

**`closeW` has no dead-lettered batches; `closeD` does (#345).** In this
section `CloseFlush.durable` stands for "the final flush completed" and
`OwedW` reads owed in this process's whole log. A background flush that
dead-lettered a batch dropped the puts in it, and before #345 the Rust still
answered such a set-aside write `intent_durable`
(`closeW_vouches_dead_lettered`). The current Rust is `closeD`, in the
section "Dead-lettered batches at close (#345)": it is `closeW` exactly when
no put was dead-lettered (`closeD_agrees`), so the theorems here describe
the current close of every such session, and `shutdown_no_silent_lossD`
restates the accounting for the others. How the lookup streak is counted is
in the section "Where the lookup-timeout streak resets (#346)".

**Writes refused at admission: a gap between the model and the Rust.** The
model starts from `WS.init (admitStore all)` and logs `admitLog all`, so
every job in `all` has a `pending` receipt and a put from the start,
including a job that `stepW`'s `.submit` later refuses (the caps did not fit
it). In the Rust a refused write answers `dropped` (settled) and gets no
intent at all. So in the model `closeW` sets such a job aside and answers it
`intent_durable` or `close_unflushed`, and `OwedW` can hold of it, though the
Rust would answer `dropped` with nothing owed. No theorem here states
anything about such a job: every one quantifies over `st.admitted`, the
writes admission accepted. `InvW.fresh` treats a refused job and one not yet
submitted alike (`pending`, untouched), which is the model's state, not the
Rust's.

`InvW` carries the argument: each settled write's receipt holds its answer
and the log its batch (or, refused after the fence, no consume); each queued
or running write's receipt is unsettled; each held write answers
`pending_replay`; none of these has a consume in the log; and a write not yet
admitted is still `pending`. It needs the admitted writes' receipt ids
distinct and every admitted write among the jobs whose intents admission
logged (`admitLog all`). The log has the shape `crash_never_reapplies` takes:
admission's puts, then the workers' batches.

Results: `shutdown_no_silent_lossW` and `shutdown_exactly_oneW` (after a
close every admitted write is exactly one of applied, failed and consumed,
failed after the fence with its intent left unconsumed, `intent_durable` and
owed in the flushed log, or `close_unflushed`; none pending),
`held_at_closeW` (a held write answers `pending_replay` while held, its
intent unconsumed, and the close answers it `intent_durable` or
`close_unflushed`), `outage_holdsW` (an outage holds the write: the current
counterpart of `outage_failed_but_owed`), `lookupTimeout_settlesW`,
`lookupTimeout_run_holdsW`, `heldErrW_holds` (a lookup timeout below the run
limit, or a held write's retry, settles `failed` and consumes; a first
attempt at the limit holds), `closeW_retry_upgradesW` (a retried close
upgrades `close_unflushed`), `runW_log_refines` (every batch the current
worker appends is one the old `workerStep` appends), `serveR_applied_pending`
(the current serve applies only stored, unconsumed rows), and
`crash_never_reappliesW` (no write in a durable prefix of the current
worker's log is applied again by the current serve). -/

/-- The receipts `abort_workers` sets aside: every one still unsettled once the
workers are aborted and joined (`pending` or `pending_replay`), held writes'
included. -/
def deferredW (st : WS) : List Key := pendingIds st.store

/-- `close()` over the current worker: `quiesce` (the trace `st` came from is
what the workers finished; `running`, `held` and `queue` are what the abort
stops), `abort_workers`, then `settle_deferred` by how the final flush ended.
It appends nothing to the log. It also sets aside a job of `all` that
admission refused, whose receipt the model leaves `pending` where the Rust
answers `dropped` (the gap in the section header); the theorems speak only of
admitted writes. `.durable` stands for "the final flush completed" and nothing
more: `closeW` reads no flush history, so it is the Rust's close only when no
background flush dead-lettered a set-aside write's put. Before #345 the Rust
was `closeW` always (`closeW_vouches_dead_lettered`); since, it is `closeD`,
which agrees with `closeW` when nothing was dead-lettered. The trace `st`
may also run a held write after the seal, which the Rust cannot (an
over-approximation; see the section header). -/
def closeW (flush : CloseFlush) (now : Int) (st : WS) : RStore :=
  settleDeferred flush now (deferredW st) st.store

/-- The answer a receipt id holds in a store. -/
def ansOf (s : RStore) (k : Key) : Option Answer := (lookup s k).map (·.answer)

/-- No batch of the log consumes `k`. -/
def Untouched (k : Key) (log : List Batch) : Prop := ∀ t, Mut.consume k t ∉ log.flatten

/-- `j`'s intent is an unconsumed row of the session's whole log, admission's
puts then the workers' batches, in the current load order (`fixedLoad`): owed
by the next serve once that log is durable. It is about this process's whole
log, not a durable prefix of it: a write's row is owed by the next serve only
if a flush took its put. `admitLog all` puts every job of `all`, so for a job
admission refused (which in the Rust gets no intent) `OwedW` can hold in the
model and not in the Rust; the theorems use it only for admitted writes. It
reads the log, not what reached the store: a write whose put a dead letter
dropped is still `OwedW` here, and has no intent in the store. Which puts
reached the store is the flush history of the section "Dead-lettered batches
at close (#345)" (`committedPuts`), where the close vouches only for those. -/
def OwedW (all : List Job) (st : WS) (j : Job) : Prop :=
  ⟨j, none⟩ ∈ pendingOf (fixedLoad (intentsOf (admitLog all ++ st.log).flatten))

/-! ### The receipts and the log, as the workers leave them -/

/-- What every reachable state of the current workers keeps: each settled
write's receipt holds its answer and its batch (or, refused after the fence,
no consume at all); each queued or running write's receipt is unsettled and
its intent untouched; each held write answers `pending_replay` with its
intent untouched; and a write of `all` not admitted is still `pending` and
untouched (`fresh`). That last clause covers alike a write not yet submitted
and one admission refused: the model gives both a `pending` receipt and a put,
where the Rust answers a refused write `dropped` and records no intent for it
(the gap in the section header). -/
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

/-! The moves, one at a time. -/

/-- The current workers' reachable states keep `InvW`. -/
abbrev Spec.sessionW_inv : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev),
    ((runW c sz now (WS.init (admitStore all)) evs).admitted.map (·.key)).Nodup →
    (∀ j ∈ (runW c sz now (WS.init (admitStore all)) evs).admitted, j ∈ all) →
    InvW all (runW c sz now (WS.init (admitStore all)) evs)

/-! ### The close -/

/-- What the close answers for a write it set aside. -/
def closeAnswer : CloseFlush → Answer
  | .durable => .intentRecorded
  | .incomplete => .closeUnflushed

/-- **Shutdown accounting over the current worker.** After any session of the
current workers (admission, holds, retries, the fence) and a `close()`, every
admitted write is one of: applied (its write and consume in one batch);
failed with its intent consumed `failed`; failed after the fence, never run,
its intent left unconsumed (the session's current holder's to apply, if an
earlier flush took it); `intent_durable` after a completed final flush, owed
in the flushed log (its row is unconsumed, so the next serve's replay owes
it), whether it was running, held or queued when the close began; or
`close_unflushed` after an incomplete flush. None is left `pending` or
`pending_replay`. `shutdown_exactly_oneW` adds that exactly one of these
holds. "Applied" means in this process's graph, its batch in the log: the
batch is durable only once a flush takes it, and one committed after the
fence is refused at the flush (`Flush/Loop.lean`). The current counterpart of
`shutdown_no_silent_loss`. -/
abbrev Spec.shutdown_no_silent_lossW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (now' : Int) (j : Job),
    j ∈ st.admitted →
    ((j, Answer.applied) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .failed ∧
      [Mut.consume j.key .failed] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧
      ansOf (closeW flush now' st) j.key = some .intentRecorded ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .incomplete ∧
      ansOf (closeW flush now' st) j.key = some .closeUnflushed)

/-- **A held write at close.** A write held in session (an outage, a run of
lookup timeouts, or behind an earlier held write of its agent) answers
`pending_replay` while held, and its intent is unconsumed; the close never
answers it `failed`: `intent_durable` after a completed final flush (and it
stays owed for the next serve's replay), `close_unflushed` otherwise. A held
write can still settle `failed` in session once it leaves `held`: its retry
meets a content refusal, or it is refused after the fence. -/
abbrev Spec.held_at_closeW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (j : Job),
    j ∈ st.held →
    ∀ (flush : CloseFlush) (now' : Int),
    ansOf st.store j.key = some .pendingReplay ∧ OwedW all st j ∧
      ansOf (closeW flush now' st) j.key = some (closeAnswer flush)

/-- **An in-session outage holds the write.** The running write of agent `a`
that meets an embedder outage becomes a held write answering `pending_replay`,
and appends nothing (its intent is not consumed). While it stays held, `held_at_closeW` says the same of every later state and of the close.
The current counterpart of `outage_failed_but_owed`, which states the worker
before #295, where the same write answered `failed`. Rust test
`a_write_reached_during_an_in_session_embedder_outage_is_not_consumed`. -/
abbrev Spec.outage_holdsW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (a : Nat) (j : Job) (t : List Job),
    ofAgent a st.running = j :: t →
    j ∈ (stepW c sz now st (.finish a (.err .unavailable))).held ∧
      ansOf (stepW c sz now st (.finish a (.err .unavailable))).store j.key = some .pendingReplay ∧
      (stepW c sz now st (.finish a (.err .unavailable))).log = st.log ∧
      OwedW all (stepW c sz now st (.finish a (.err .unavailable))) j

/-- The error the model hands `finish` for a run that failed with `e`: the
worker's hold decision in `spawn_worker`, before its counters move. A lookup
timeout on a write's first attempt adds one to `lookup_streak` (lookup
timeouts in a row, across lanes, with nothing applied between; `streak` is its
value before), and every one that brings it to `REPLAY_LOOKUP_TIMEOUT_LIMIT`
or finds it past it (the third, and each later one until a write applies and
resets it) holds the write as an outage does; a retried held write's lookup timeout does
not count and settles. This is the abstraction `stepW`'s header describes
("handed to the model as `unavailable`"), now as a function. The streak is an
input here; how the worker counts it, reset at each commit (#346), is the
section "Where the lookup-timeout streak resets (#346)" (`streakNew`,
`heldNew_iff`). -/
def heldErrW (retried : Bool) (streak : Nat) : Err → Err
  | .lookupTimeout =>
    if retried = false ∧ lookupTimeoutLimit ≤ streak + 1 then .unavailable else .lookupTimeout
  | e => e

/-- The worker holds exactly on an outage or on a first-attempt lookup timeout
that brings, or finds, the run of them at `REPLAY_LOOKUP_TIMEOUT_LIMIT` or
past it. -/
abbrev Spec.heldErrW_holds : Prop :=
  ∀ (retried : Bool) (streak : Nat) (e : Err),
    holdsS (heldErrW retried streak e) = true ↔
      (e = .unavailable ∨ (e = .lookupTimeout ∧ retried = false ∧ lookupTimeoutLimit ≤ streak + 1))

/-- **A single lookup timeout settles `failed` and consumes the intent**
(below the run limit, or the retry of a held write): the write is settled
`failed` and its batch is the consume. Rust test
`a_lookup_timeout_in_session_settles_failed_and_holds_nothing`. -/
abbrev Spec.lookupTimeout_settlesW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (a : Nat) (j : Job) (t : List Job),
    ofAgent a st.running = j :: t →
    ∀ (retried : Bool) (streak : Nat),
    retried = true ∨ streak + 1 < lookupTimeoutLimit →
    let st' := stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout)))
    ansOf st'.store j.key = some .failed ∧ (j, Answer.failed) ∈ st'.settled ∧
      st'.log = st.log ++ [[Mut.consume j.key .failed]]

/-- **A run of lookup timeouts holds the write**: a first-attempt lookup
timeout once the run has reached the limit is held, as an outage
does (`outage_holdsW`). Rust test
`a_lookup_timeout_run_holds_the_third_write_and_its_retry_settles_it`. -/
abbrev Spec.lookupTimeout_run_holdsW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (a : Nat) (j : Job) (t : List Job),
    ofAgent a st.running = j :: t →
    ∀ (streak : Nat),
    lookupTimeoutLimit ≤ streak + 1 →
    let st' := stepW c sz now st (.finish a (.err (heldErrW false streak .lookupTimeout)))
    j ∈ st'.held ∧ ansOf st'.store j.key = some .pendingReplay ∧ st'.log = st.log ∧
      OwedW all st' j

/-- **A retried close upgrades `close_unflushed`.** A write the close set aside
(running, held or queued) answers `close_unflushed` after an incomplete flush;
a retried close whose flush completes settles the same set-aside ids
(`close_deferred`, kept after an incomplete flush) and answers it
`intent_durable`. `retry_upgrades` is the same fact for one entry. -/
abbrev Spec.closeW_retry_upgradesW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (j : Job),
    j ∈ st.running ++ st.held ++ st.queue →
    ∀ (now1 now2 : Int),
    ansOf (closeW .incomplete now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferred .durable now2 (deferredW st) (closeW .incomplete now1 st)) j.key =
        some .intentRecorded

/-! ### Crash and the serve that follows -/

/-- **The refinement.** Every batch the current workers append is a batch the
old worker (`workerStep`, unfenced) appends for some write and run result: an
applied write's is `workerStep`'s success batch, a consumed failure's is its
`[consume failed]` (for a lookup timeout the old worker appended nothing; the
batch is the one it appends for a content refusal), and a hold or a fenced
refusal appends none. So `workerStep_atomic` and `workerStep_keys` hold of the
current worker's batches, which is what the crash argument needs. -/
abbrev Spec.runW_log_refines : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), (∀ b ∈ st.log, ∃ j r, b = (workerStep false j r).2) →
      ∀ b ∈ (runW c sz now st evs).log, ∃ j r, b = (workerStep false j r).2

/-- **The current serve applies only stored, unconsumed rows.** Every write
`serveR` (both passes, the block reason after a halt) applies is a row it was
given, unconsumed: the first pass by `replayD_facts`, the retry pass by
`loopD_facts` over the rows the first pass's consumes left unconsumed
(`foldl_pending_mem`). The current counterpart of `replay_applied_pending`. -/
abbrev Spec.serveR_applied_pending : Prop :=
  ∀ {live : Bool} {rout rout' : Job → RR} {s' : Streaks} {intents : List Intent} {x : Job},
    x ∈ (serveR live rout rout' s' intents).applied →
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none

/-- **No write is applied twice across a crash, over the current worker and
serve.** A flush makes some prefix of the session's log durable (admission's
puts, then the current workers' batches), and the process may die after any
flush. If a write is in the durable prefix, its consume is in the same batch
(`runW_log_refines`, `workerStep_atomic`), so the next serve (`serveR` over
the current load order, both passes) never applies it again. The current
counterpart of `crash_never_reapplies`; like it, about one process's log over
an empty intent table. -/
abbrev Spec.crash_never_reappliesW : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore) (evs : List Ev) (all : List Job) (n : Nat)
    (r : Key) (live : Bool) (rout rout' : Job → RR) (s' : Streaks),
    Mut.write r ∈ ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten →
    ∀ x ∈ (serveR live rout rout' s' (fixedLoad (intentsOf
      ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten))).applied,
      x.key ≠ r

/-! ### Exactly one fate at close -/

/-- The five cases of `shutdown_no_silent_lossW`, by name. -/
inductive FateW where
  | applied
  | failedConsumed
  | failedFenced
  | durable
  | unflushed
  deriving DecidableEq, Repr

/-- What each case says of write `j` after `closeW flush now'` over `st`. -/
def FateW.holds (all : List Job) (st : WS) (flush : CloseFlush) (now' : Int) (j : Job) :
    FateW → Prop
  | .applied => (j, Answer.applied) ∈ st.settled ∧
      ansOf (closeW flush now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log
  | .failedConsumed => (j, Answer.failed) ∈ st.settled ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ [Mut.consume j.key .failed] ∈ st.log
  | .failedFenced => (j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ OwedW all st j
  | .durable => j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧
      ansOf (closeW flush now' st) j.key = some .intentRecorded ∧ OwedW all st j
  | .unflushed => j ∈ st.running ++ st.held ++ st.queue ∧ flush = .incomplete ∧
      ansOf (closeW flush now' st) j.key = some .closeUnflushed

/-- **Exactly one fate at close.** For every admitted write, exactly one case
of `shutdown_no_silent_lossW` holds. The cases differ by the close's answer,
except the two `failed` ones: a failure consumed in session has its
`[consume failed]` batch in the log, so its intent is not owed, while a write
refused after the fence is owed. -/
abbrev Spec.shutdown_exactly_oneW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (now' : Int) (j : Job),
    j ∈ st.admitted →
    (∃ f : FateW, f.holds all st flush now' j) ∧
      ∀ f g : FateW, f.holds all st flush now' j → g.holds all st flush now' j → f = g

/-! ## Dead-lettered batches at close (#345)

`closeW` takes how the final flush ended (`CloseFlush`) and nothing else, and
answers every set-aside write `intent_durable` when it is `.durable`: in the
Rust, `Memory::close` passes `CloseFlush::Durable` when the final flush
completes and when the tail is empty ("every set-aside intent was taken by an
earlier flush, so it is durable"). Neither says the set-aside write's put
reached the store. A background flush that the store refuses with a
constraint violation, or a value it cannot hold, dead-letters its batch: the
whole of `pending`, dropped while the session goes on (`Flush/Loop.lean`,
`constraint_dead_lettered`, `value_rejected_dead_letters`). That batch can
hold the `PutWriteIntent` of a write that is still unsettled (a held,
`pending_replay` one, or one still queued), and then nothing durable backs
its `intent_durable`.

This section adds the flush history the close ran after, and states the
close's answer against it.

* **The history** (`Landed`, one per batch of the session's log,
  `sessLog all st`, in log order): `committed` or `deadLettered`. It is the
  history as a close whose final flush completed (or found an empty tail)
  sees it: every batch of the log has left the process, through a background
  flush, the stop's push-back and the final flush, or a dead letter
  (`hist.length = (sessLog all st).length`). A flush carries whole batches
  and a dead letter drops all of `pending`, so how the batches were grouped
  into flushes does not change which puts reached the store; the model keeps
  only each batch's fate. A degraded session's drops and a fenced session's
  drops are not in it: the close answers both `close_unflushed` whatever the
  history (`CloseFlush::Incomplete`, `Memory::close`, `close_fenced`).
* **"Durable" means committed** (`committedPuts`): the write's put is in a
  batch the store committed, not merely one a flush drained.
* **The rule before #345 is wrong** (`closeW_vouches_dead_lettered`, a concrete
  case in `Examples.lean`, and `closeW_vouches_dead_letteredW`, every
  reachable session): a
  set-aside write whose put a dead letter dropped answers `intent_durable` at
  a close whose flush completed, and its put is not committed.
* **The fixed rule** (`closeD`, `settleDeferredD`; Rust: the flush records
  the receipt ids of the puts in every batch it dead-letters,
  `Graph::note_dead_lettered_intents`, and `settle_deferred` answers such a
  receipt `close_unflushed` even under `CloseFlush::Durable`): a set-aside
  write answers `intent_durable` only when the flush ended `.durable` and its
  put is not among the dead-lettered ones (`flushFor`). At a close whose
  final flush completed it is exact, not conservative: `closeD_durable_iff`
  (a set-aside write answers `intent_durable` exactly when its put is
  committed). A close whose flush did not complete answers every set-aside
  write `close_unflushed`, a committed put included, as before #345. The
  other theorems: `closeD_vouches_only_committed`,
  `closeD_never_vouches_dropped` (whatever the flush outcome), and
  `shutdown_no_silent_lossD`, the accounting of `shutdown_no_silent_lossW`
  with the dead-lettered writes moved to `close_unflushed`. A retried close
  keeps the rule (`closeD_retryD`): it upgrades `close_unflushed` only for a
  write whose put was not dead-lettered. Where no batch was dead-lettered
  the fixed close is the old one (`closeD_agrees`).

Exactness rests on the receipt ids of the puts being distinct
(`(all.map (·.key)).Nodup`; the Rust's ids are unique per process and the put
is an upsert on the id), and on the worker's batches carrying no put
(`runW_noPut`). -/

/-- How one batch of the session's log left the process before the close:
committed by a flush (a background one or the close's final one), or
dropped as a dead letter. -/
inductive Landed where
  | committed
  | deadLettered
  deriving DecidableEq, Repr

/-- The receipt ids whose `PutWriteIntent` is in batch `b`. -/
def putKeys (b : Batch) : List Key :=
  b.filterMap (fun m => match m with
    | .put j => some j.key
    | _ => none)

/-- The receipt ids whose put is in a batch of `log` that `hist` (one fate per
batch, in log order) says landed `f`. -/
def keysLanded (log : List Batch) (hist : List Landed) (f : Landed) : List Key :=
  ((log.zip hist).filter (fun p => decide (p.2 = f))).flatMap (fun p => putKeys p.1)

/-- The puts a dead letter dropped: what the Rust's flush records
(`Graph::note_dead_lettered_intents`). -/
def droppedPuts (log : List Batch) (hist : List Landed) : List Key :=
  keysLanded log hist .deadLettered

/-- The puts the store committed. -/
def committedPuts (log : List Batch) (hist : List Landed) : List Key :=
  keysLanded log hist .committed

/-- The session's whole log: admission's puts, then the workers' batches. -/
def sessLog (all : List Job) (st : WS) : List Batch := admitLog all ++ st.log

/-- The flush outcome the fixed close settles receipt `k` by: a write whose
put a dead letter dropped is settled as after an incomplete flush. -/
def flushFor (flush : CloseFlush) (dropped : List Key) (k : Key) : CloseFlush :=
  if k ∈ dropped then .incomplete else flush

/-- `settle_deferred` after #345: each set-aside receipt by `flushFor`. -/
def settleDeferredD (flush : CloseFlush) (dropped : List Key) (now : Int) (ids : List Key)
    (s : RStore) : RStore :=
  ids.foldl (fun s r => update s r (deferEntry (flushFor flush dropped r) now)) s

/-- `close()` after #345: `closeW` with the dead-lettered puts. -/
def closeD (flush : CloseFlush) (dropped : List Key) (now : Int) (st : WS) : RStore :=
  settleDeferredD flush dropped now (deferredW st) st.store

/-! ### The fixed close's receipts -/

/-- **With no dead letter, the fixed close is the old one.** -/
abbrev Spec.closeD_agrees : Prop :=
  ∀ (flush : CloseFlush) (now : Int) (st : WS) (k : Key),
    ansOf (closeD flush [] now st) k = ansOf (closeW flush now st) k

/-! ### The rule before #345 is wrong -/

/-- **#345, in general: the close before the fix (`closeW`) vouches for a
dead-lettered put.** In every reachable session, a set-aside write (running,
held or queued when the close began) whose put the history dead-lettered
answers `intent_durable` at a `closeW` whose flush completed, though its put
is not committed. The current close is `closeD`, which does not
(`closeD_never_vouches_dropped`). -/
abbrev Spec.closeW_vouches_dead_letteredW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    (all.map (·.key)).Nodup →
    ∀ (hist : List Landed) (now' : Int) {j : Job},
    j ∈ st.running ++ st.held ++ st.queue →
    j.key ∈ droppedPuts (sessLog all st) hist →
    ansOf (closeW .durable now' st) j.key = some .intentRecorded ∧
      j.key ∉ committedPuts (sessLog all st) hist

/-! ### The fixed rule -/

/-- **Shutdown accounting under the fixed close (#345).** As
`shutdown_no_silent_lossW`, except that a set-aside write answers
`intent_durable` only when the flush completed and its put was not
dead-lettered, and `close_unflushed` when the flush was incomplete or its put
was dead-lettered. -/
abbrev Spec.shutdown_no_silent_lossD : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (dropped : List Key) (now' : Int) (j : Job),
    j ∈ st.admitted →
    ((j, Answer.applied) ∈ st.settled ∧
      ansOf (closeD flush dropped now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ ansOf (closeD flush dropped now' st) j.key = some .failed ∧
      [Mut.consume j.key .failed] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeD flush dropped now' st) j.key = some .failed ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧ j.key ∉ dropped ∧
      ansOf (closeD flush dropped now' st) j.key = some .intentRecorded ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ (flush = .incomplete ∨ j.key ∈ dropped) ∧
      ansOf (closeD flush dropped now' st) j.key = some .closeUnflushed)

/-- **The fixed close never vouches for a dropped put.** An admitted write
whose put a dead letter dropped never answers `intent_durable`, whatever the
flush outcome. -/
abbrev Spec.closeD_never_vouches_dropped : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (hist : List Landed) (now' : Int) (j : Job),
    j ∈ st.admitted →
    j.key ∈ droppedPuts (sessLog all st) hist →
    ansOf (closeD flush (droppedPuts (sessLog all st) hist) now' st) j.key ≠
      some .intentRecorded

/-- **The fixed close vouches only for committed puts.** Once every batch of
the log has landed (a completed or empty final flush), an admitted write
that answers `intent_durable` has its put in a committed batch. -/
abbrev Spec.closeD_vouches_only_committed : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (hist : List Landed) (now' : Int) (j : Job),
    j ∈ st.admitted →
    hist.length = (sessLog all st).length →
    ansOf (closeD flush (droppedPuts (sessLog all st) hist) now' st) j.key =
      some .intentRecorded →
    j.key ∈ committedPuts (sessLog all st) hist

/-- **The fixed rule is exact.** At a close whose flush completed (every
batch landed), a set-aside write answers `intent_durable` exactly when its
put is committed, and `close_unflushed` otherwise. -/
abbrev Spec.closeD_durable_iff : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    (all.map (·.key)).Nodup →
    ∀ (hist : List Landed) (now' : Int),
    hist.length = (sessLog all st).length →
    ∀ {j : Job},
    j ∈ st.running ++ st.held ++ st.queue →
    (ansOf (closeD .durable (droppedPuts (sessLog all st) hist) now' st) j.key =
        some .intentRecorded ↔ j.key ∈ committedPuts (sessLog all st) hist) ∧
      (j.key ∉ committedPuts (sessLog all st) hist →
        ansOf (closeD .durable (droppedPuts (sessLog all st) hist) now' st) j.key =
          some .closeUnflushed)

/-- **A retried close keeps the rule.** A set-aside write answers
`close_unflushed` after an incomplete flush; a retried close whose flush
completes upgrades it to `intent_durable` only if its put was not
dead-lettered, and leaves it `close_unflushed` otherwise (the Rust's
upgrade in `settle_deferred` is skipped for a dead-lettered receipt). -/
abbrev Spec.closeD_retryD : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (dropped : List Key) {j : Job},
    j ∈ st.running ++ st.held ++ st.queue →
    ∀ (now1 now2 : Int),
    ansOf (closeD .incomplete dropped now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferredD .durable dropped now2 (deferredW st)
          (closeD .incomplete dropped now1 st)) j.key =
        some (if j.key ∈ dropped then .closeUnflushed else .intentRecorded)

/-! ## Worker panics and the dead-lettered set (#345 review)

`stepW` has no panic. In the Rust a lane worker can panic after its commit
(the graph write and the intent's consume, under one graph lock hold) and
before `settle_one`: in the index mirroring, the receipt summary or the
ledger append. Its receipt stays unsettled (`Lanes::worker_alive`), and the
close sets it aside like a running write. This focused model follows one
session's receipts through the steps that touch the dead-lettered set
(`Graph::dead_lettered_intents`):

* `consume k`: the write's consume is appended (its commit, or a failure's
  consume), under the graph lock;
* `settle k`: the worker's settle block, after `settle_one`;
* `deadLetter ks`: a background flush dead-letters a batch whose puts are
  `ks`. A put precedes its consume in log order and the dropped batch is the
  flush's only pending one, so a consume of `k` appended before this step is
  in that batch or still in the log, which is what the Rust's note-time skip
  looks at.

A panic is a `consume k` with no `settle k` after it. At close, a receipt not
settled is set aside, and a close whose final flush completed answers it
`intent_durable` exactly when it is not in the set.

* `stepK` (the rule at `1b4d89e4`): a consume forgets the receipt, and the
  note skips a put already consumed. `keptAtConsume_vouches_after_panic`
  (`decide`): a write whose put was dead-lettered, then consumed, then whose
  worker panicked, is set aside and out of the set, so the close vouches for
  it; the same when the consume rode the dropped batch.
* `stepP` (the current Rust): a worker's consume also records the receipt as
  consumed and not yet settled (`Graph::consumed_unsettled`); the settle
  block forgets it from both sets; the note skips a put only when its write
  is consumed and no longer in that record, that is, settled. The Rust
  checks "consumed" as "its consume is in the dropped batch or still in the
  mutation log" (`note_dead_lettered_intents`), not over the whole history
  as `consumed` does here. The two agree on every put the note sees: a put
  precedes its consume in log order, and the dropped batch is the flush
  loop's only pending one, so a consume that already left the log in an
  earlier batch had its put in that batch or before it, never in the batch
  being dropped.
  `settlePrune_never_vouches_dropped`: over every trace, a dropped put whose
  write is not settled is in the set, so no close vouches for it, panics
  included. `settlePrune_exact`: for a write not settled, it is in the set
  exactly when its put was dropped, so the close's exactness
  (`closeD_durable_iff`) carries over. `settlePrune_settled_forgotten`: a
  settled write whose consume came before its settle, as the worker's always
  does, leaves the set at its settle and is not noted again, so the set
  holds only writes not yet settled, or never consumed (refused after a lost
  lease, whose close answers `close_unflushed` anyway).
  `settlePrune_unsettled_bounded`: the record of consumed, unsettled writes
  holds only such writes: at most one per worker between its commit and its
  settle, plus one per panicked worker. -/

/-- A step that touches the dead-lettered set. -/
inductive DEv where
  | consume (k : Key)
  | settle (k : Key)
  | deadLetter (ks : List Key)
  deriving DecidableEq, Repr

/-- The bookkeeping: the set (`dl`), the consumed-and-unsettled record
(`cu`), and, for the statements, which writes were consumed and settled and
which puts were dropped. -/
structure DS where
  dl : List Key
  cu : List Key
  consumed : List Key
  settled : List Key
  dropped : List Key
  deriving DecidableEq, Repr

def DS.init : DS := ⟨[], [], [], [], []⟩

/-- The rule at `1b4d89e4`: prune at the consume, skip a consumed put at the
note. -/
def stepK (s : DS) : DEv → DS
  | .consume k => { s with consumed := k :: s.consumed, dl := s.dl.filter (· ≠ k) }
  | .settle k => { s with settled := k :: s.settled }
  | .deadLetter ks =>
    { s with dropped := ks ++ s.dropped, dl := ks.filter (fun k => k ∉ s.consumed) ++ s.dl }

/-- The current rule: prune at the settle; skip at the note only a put whose
write is consumed and settled (consumed, and out of `cu`). -/
def stepP (s : DS) : DEv → DS
  | .consume k => { s with consumed := k :: s.consumed, cu := k :: s.cu }
  | .settle k =>
    { s with settled := k :: s.settled, cu := s.cu.filter (· ≠ k), dl := s.dl.filter (· ≠ k) }
  | .deadLetter ks =>
    { s with dropped := ks ++ s.dropped,
             dl := ks.filter (fun k => k ∉ s.consumed ∨ k ∈ s.cu) ++ s.dl }

/-- What a close whose final flush completed answers a set-aside receipt
`k` (one not settled), given the set. -/
def vouches (s : DS) (k : Key) : Bool := decide (k ∉ s.settled) && decide (k ∉ s.dl)

private def kP : Key := (0, 1000, 1)

/-- **#345 review: pruning at the consume vouches for a dropped put after a
worker panic.** The write's put is dead-lettered while it is queued; it
commits (its consume is appended, which forgets it); its worker panics
before the settle. At close it is set aside and out of the set, so the close
answers `intent_durable`, though its put was dropped. Second case: the put
and the consume ride the dropped batch together, and the note skips it. The
current rule keeps both in the set. Rust test
`a_worker_that_panics_after_its_commit_does_not_get_intent_durable`. -/
abbrev Spec.keptAtConsume_vouches_after_panic : Prop :=
  let t1 := [DEv.deadLetter [kP], .consume kP]
  let t2 := [DEv.consume kP, .deadLetter [kP]]
  (t1.foldl stepK DS.init).dropped = [kP] ∧ vouches (t1.foldl stepK DS.init) kP = true ∧
    (t2.foldl stepK DS.init).dropped = [kP] ∧ vouches (t2.foldl stepK DS.init) kP = true ∧
    vouches (t1.foldl stepP DS.init) kP = false ∧ vouches (t2.foldl stepP DS.init) kP = false

/-- The step a worker can take: it never consumes a write it already
settled (a write is consumed at most once, before its settle). -/
def DEv.ok (s : DS) : DEv → Prop
  | .consume k => k ∉ s.settled
  | _ => True

/-- Every step of a worker trace keeps the invariant. -/
def DOk : DS → List DEv → Prop
  | _, [] => True
  | s, e :: es => e.ok s ∧ DOk (stepP s e) es

/-- **The current rule never vouches for a dropped put, panics included.**
Over every worker trace, a write whose put was dead-lettered and that is not
settled (set aside by the close, a panicked worker's included) is in the
set, so the close answers it `close_unflushed`. -/
abbrev Spec.settlePrune_never_vouches_dropped : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∈ (evs.foldl stepP DS.init).dropped →
    vouches (evs.foldl stepP DS.init) k = false

/-- **The current rule is exact for the writes a close sets aside.** For a
write not settled, it is in the set exactly when its put was dropped. -/
abbrev Spec.settlePrune_exact : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∉ (evs.foldl stepP DS.init).settled →
    (k ∈ (evs.foldl stepP DS.init).dl ↔ k ∈ (evs.foldl stepP DS.init).dropped)

/-- **The record of consumed, unsettled writes is bounded by them**: every
entry is a write consumed and not settled (in the Rust, one per worker
between its commit and its settle, plus one per panicked worker). -/
abbrev Spec.settlePrune_unsettled_bounded : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∈ (evs.foldl stepP DS.init).cu →
    k ∈ (evs.foldl stepP DS.init).consumed ∧ k ∉ (evs.foldl stepP DS.init).settled

/-- After a settle, a consumed write is out of the set and out of the
record, and a later dead letter of its put does not note it. -/
abbrev Spec.settlePrune_settled_forgotten : Prop :=
  ∀ (s : DS) (k : Key),
    k ∈ s.consumed →
    ∀ (ks : List Key),
    k ∉ (stepP s (.settle k)).dl ∧ k ∉ (stepP s (.settle k)).cu ∧
      k ∉ (stepP (stepP s (.settle k)) (.deadLetter ks)).dl

/-! ## Where the lookup-timeout streak resets (#346)

`heldErrW` takes `lookup_streak` as an input. The rule it serves (the docs,
`spawn_worker`'s comment): a first-attempt lookup timeout is held once it is
at least the `REPLAY_LOOKUP_TIMEOUT_LIMIT`-th of a run of them **with nothing
applied between**; any other settles `failed`. This section counts the
streak.

The events are the steps that read or write `lookup_streak`, across every
lane, in the order they happen:

* `timeout`: a first-attempt lookup timeout (`spawn_worker`, the
  `is_lookup_timeout` arm) takes the lanes lock and adds one, and the write
  is held when the new value reaches the limit (`heldErrW false streak`,
  `streak` the value before). A held write's retry that times out does not
  touch the streak and is no event here.
* `commit`: a write's commit, inside `WriteCtx::run`, under the graph write
  lock: its mutations and the consume of its intent (the hybrid
  `CommitHook`, or the canonical and action paths' inline blocks). Since the
  fix the reset happens here too, the lanes lock taken under the graph write
  lock (the order admission nests them in), so the commit and the reset are
  one event: the commit becomes visible only when the graph lock is
  released, after the reset, and a timeout's lanes-lock hold falls either
  before the reset (then before the commit, as the graph lock publishes it)
  or after both.
* `okArm`: the worker's `Ok` arm, after `WriteCtx::run` has returned: later
  than the commit by the index mirroring (`mirror_concepts`, which waits for
  the graph read and index write locks), the daemon wake and the receipt
  summary. No `.await` lies between, but another worker can take the lanes
  lock in that stretch on a multi-threaded runtime (and, in the Rust test, at
  the test-only `pause_after_commit`).
* `settleOk`: that write's settle block, later still in the same iteration
  (in the Rust test, after the test-only `pause_before_settle`).

"Applied" in the rule is the commit.

* `runSince`: the documented count, the timeouts since the last commit.
* `streakOld`, the Rust before #346, which reset the streak in the settle
  block: `streak_old_counts_across_commit`, a timeout that follows a commit
  but comes before that write's settle is held as the third of a run (two
  timeouts came before the commit), where the rule says it is a first timeout
  and settles `failed`.
* `streakOkArm`, #346's first fix, which reset in the `Ok` arm:
  `streak_okArm_counts_across_commit`, the same fault in the narrower stretch
  between the commit and the `Ok` arm (found in review).
* `streakBoth`, resets at the commit and the settle: wrong the other way
  (`streak_both_forgets_run`), the settle's reset forgets timeouts that came
  after the commit. So the fix keeps no later reset.
* `streakNew`, the fix (reset at the commit, in its critical section, only):
  `streakNew_eq_runSince` over every trace; `heldNew_iff`: a first-attempt
  timeout is held exactly when `limit ≤ runSince + 1`;
  `streakNew_after_commit`, `timeout_after_commit_settlesW`: a first-attempt
  timeout with no timeout since the last commit counts as a first timeout,
  and in the worker model settles `failed` and consumes its intent. -/

/-- What moves `lookup_streak`, or could, in the order it happens. -/
inductive SEv where
  | commit
  | okArm
  | settleOk
  | timeout
  deriving DecidableEq, Repr

/-- The streak before #346: reset in the settle block. -/
def streakOld (s : Nat) : SEv → Nat
  | .commit => s
  | .okArm => s
  | .settleOk => 0
  | .timeout => s + 1

/-- #346's first fix: reset in the worker's `Ok` arm, after `WriteCtx::run`
returned. -/
def streakOkArm (s : Nat) : SEv → Nat
  | .commit => s
  | .okArm => 0
  | .settleOk => s
  | .timeout => s + 1

/-- The streak after the fix: reset at the commit, in its critical section. -/
def streakNew (s : Nat) : SEv → Nat
  | .commit => 0
  | .okArm => s
  | .settleOk => s
  | .timeout => s + 1

/-- Resets at the commit and the settle, which the fix does not keep. -/
def streakBoth (s : Nat) : SEv → Nat
  | .commit => 0
  | .okArm => s
  | .settleOk => 0
  | .timeout => s + 1

/-- The documented count: the lookup timeouts since the last write applied. -/
def runSince (evs : List SEv) : Nat :=
  ((evs.reverse.takeWhile (fun e => e != .commit)).filter (fun e => e == .timeout)).length

/-- **The fixed streak is the documented count**, over every trace. -/
abbrev Spec.streakNew_eq_runSince : Prop :=
  ∀ (evs : List SEv),
    evs.foldl streakNew 0 = runSince evs

/-- **The fixed worker holds a first-attempt lookup timeout exactly when the
documented run reaches the limit.** -/
abbrev Spec.heldNew_iff : Prop :=
  ∀ (evs : List SEv),
    holdsS (heldErrW false (evs.foldl streakNew 0) .lookupTimeout) = true ↔
      lookupTimeoutLimit ≤ runSince evs + 1

/-- **A timeout after an applied write counts as a first timeout** under the
fixed streak, whatever came before the commit and whatever settles after it,
as long as no other timeout came since. -/
abbrev Spec.streakNew_after_commit : Prop :=
  ∀ (pre mid : List SEv),
    (∀ e ∈ mid, e ≠ .timeout) →
    (pre ++ SEv.commit :: mid).foldl streakNew 0 = 0

/-- **In the worker model, that timeout settles `failed`**: the running write
of agent `a` whose first attempt times out after a commit (no timeout since)
is settled `failed` with its intent consumed, by `lookupTimeout_settlesW` at
the streak the fixed worker counts. Rust tests
`a_lookup_timeout_between_a_commit_and_its_settle_settles_failed`,
`a_lookup_timeout_between_a_commit_and_its_ok_arm_settles_failed`. -/
abbrev Spec.timeout_after_commit_settlesW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (a : Nat) (j : Job) (t : List Job),
    ofAgent a st.running = j :: t →
    ∀ (pre mid : List SEv),
    (∀ e ∈ mid, e ≠ .timeout) →
    let st' := stepW c sz now st
      (.finish a (.err (heldErrW false ((pre ++ SEv.commit :: mid).foldl streakNew 0) .lookupTimeout)))
    ansOf st'.store j.key = some .failed ∧ (j, Answer.failed) ∈ st'.settled ∧
      st'.log = st.log ++ [[Mut.consume j.key .failed]]

/-- **#346: the streak before the fix counts a timeout that follows a
commit.** Two lookup timeouts, then another lane's write commits; before
that write's settle block resets the streak, a third first-attempt timeout
takes the lanes lock. The old streak reads 2 and holds it as the third of a
run; nothing was timed out since the commit (`runSince` is 0), so the rule
says it is a first timeout, and the fixed streak settles it `failed`. Rust
test `a_lookup_timeout_between_a_commit_and_its_settle_settles_failed`. -/
abbrev Spec.streak_old_counts_across_commit : Prop :=
  let pre := [SEv.timeout, .timeout, .commit]
  holdsS (heldErrW false (pre.foldl streakOld 0) .lookupTimeout) = true ∧
    runSince pre = 0 ∧
    holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = false

/-- **#346's first fix still counted a timeout that follows a commit.** It
reset the streak in the worker's `Ok` arm, which runs after `WriteCtx::run`
returns: after the index mirroring, which waits for the graph read and index
write locks. Two lookup timeouts, then another lane's write commits; before
its `Ok` arm runs, a third first-attempt timeout takes the lanes lock. The
`Ok`-arm streak reads 2 and holds it; `runSince` is 0, and the fixed streak,
reset inside the commit, settles it `failed`. Rust test
`a_lookup_timeout_between_a_commit_and_its_ok_arm_settles_failed`. -/
abbrev Spec.streak_okArm_counts_across_commit : Prop :=
  let pre := [SEv.timeout, .timeout, .commit]
  holdsS (heldErrW false (pre.foldl streakOkArm 0) .lookupTimeout) = true ∧
    runSince pre = 0 ∧
    holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = false

/-- **Keeping the settle's reset as well is wrong the other way.** A write
commits, two timeouts follow, then its settle runs: with both resets the
streak reads 0 and the third timeout settles `failed`, though it is the third
with nothing applied between (`runSince` is 2) and the rule holds it, as the
fixed streak does. -/
abbrev Spec.streak_both_forgets_run : Prop :=
  let pre := [SEv.commit, .timeout, .timeout, .settleOk]
  holdsS (heldErrW false (pre.foldl streakBoth 0) .lookupTimeout) = false ∧
    runSince pre = 2 ∧
    holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = true

end Lambo.Drain
