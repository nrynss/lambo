/-
# Write-queue drain: the close, a crash, and the lookup-timeout streak

What `close()` answers for each admitted write after any trace of
`Worker.lean`'s worker (`closeW`), and against the flush history of which
puts a dead letter dropped (`closeD`); what the serve after a crash applies;
the dead-lettered set's bookkeeping with worker panics (`stepP`); and how the
lookup-timeout streak counts (`streakNew`).

Rust: `quiesce`, `abort_workers`, `settle_deferred` (`src/writeq/drain.rs`);
`Memory::close`, `Memory::finish_close`, `Memory::close_fenced`
(`src/memory/shutdown.rs`); `consume_write_intent`
(`src/graph/graph/mutation_log.rs`); `Graph::note_dead_lettered_intents`,
`intent_dead_lettered`, `mark_consumed_unsettled`, `settled_write`
(`src/graph/graph/unflushed.rs`); `FlushLoop::drain_and_flush`
(`src/store/flush.rs`); `WriteCtx::run`, `WorkerCommit`, `spawn_worker`
(`src/writeq/execution.rs`); `Lanes` (`src/writeq/admission.rs`);
`settle_one` (`src/writeq/receipts.rs`).

Results:
* `shutdown_no_silent_lossD`: after a close every admitted write is applied, failed,
  `intent_durable` and owed, or `close_unflushed`; none is left pending.
* `shutdown_exactly_oneW`: with no dead letter, exactly one of those fates holds.
* `held_at_closeW`, `outage_holdsW`: an outage holds a write, which the close never answers `failed`.
* `heldErrW_holds`, `lookupTimeout_settlesW`, `lookupTimeout_run_holdsW`, `heldNew_iff`,
  `timeout_after_commit_settlesW`: a lookup timeout settles `failed` unless it is a first
  attempt at least the limit-th since the last commit, which holds.
* `serveR_applied_pending`, `crash_never_reappliesW`: the serve applies only unconsumed
  rows, so never a write that survived a crash.
* `closeD_agrees`, `closeD_durable_iff`, `closeD_vouches_only_committed`,
  `closeD_never_vouches_dropped`, `closeD_retryD`: the close vouches exactly for
  committed puts, retried or not.
* `settlePrune_never_vouches_dropped`, `settlePrune_exact`, `settlePrune_unsettled_bounded`,
  `settlePrune_settled_forgotten`: the dead-lettered set never lets the close vouch for a
  dropped put, panics included.

Boundary: each step is atomic. Which batches the store committed is an input
(`Landed`): the flush loop is `Flush/`, and the verdict also rests on the
flush task's join (`Flush.close_after_flush_panic_cannot_vouch`); the lease
fence at the flush is not modelled, and a degraded or fenced session's drops
are not in the history (the Rust passes `Incomplete` for both). The model gives every job of `all` a
`pending` receipt and a put, one admission refuses included, where the Rust
answers `dropped` with no intent; the results speak only of admitted writes.
`stepW` has no seal, so a trace standing for the drain may retry a held write
after `quiesce` seals the lanes, which the Rust cannot: the model has more
traces than the Rust, never fewer. The flush outcome is a parameter, so a
fenced session's `closeW .durable` answers `intent_durable`, which the Rust's
`close_fenced` (always `Incomplete`) never does: that a fenced close answers
`close_unflushed` is code-backed, not proved. `stepW` has no panic; a worker
that panics between its commit and its settle is the `DEv` model's.
-/

import LamboProofs.Drain.Spec.Worker

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The close -/

/-- The receipts `abort_workers` sets aside: every one still unsettled
(`pending` or `pending_replay`). -/
def deferredW (st : WS) : List Key := pendingIds st.store

/-- `close()` (`Memory::close`): `abort_workers`, then `settle_deferred` by how
the final flush ended, after any `runW` trace (a write still running is one
the abort stopped before its commit). It reads no flush history; `closeD` does. -/
def closeW (flush : CloseFlush) (now : Int) (st : WS) : RStore :=
  settleDeferred flush now (deferredW st) st.store

/-- The answer a receipt id holds in a store. -/
def ansOf (s : RStore) (k : Key) : Option Answer := (lookup s k).map (·.answer)

/-- `j`'s intent is an unconsumed row of the session's whole log, in load order:
owed by the next serve once a flush takes its put. It reads the log, not
what reached the store (`committedPuts`). -/
def OwedW (all : List Job) (st : WS) (j : Job) : Prop :=
  ⟨j, none⟩ ∈ pendingOf (fixedLoad (intentsOf (admitLog all ++ st.log).flatten))

/-- What the close answers for a write it set aside. -/
def closeAnswer : CloseFlush → Answer
  | .durable => .intentRecorded
  | .incomplete => .closeUnflushed

/-- **A held write answers `pending_replay`, stays owed, and the close answers it
`intent_durable` or `close_unflushed`**, never `failed`. -/
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

/-- **An in-session outage holds the write**: it answers `pending_replay`,
appends nothing and stays owed. Rust test
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

/-! ## Held writes and lookup timeouts -/

/-- The error `finish` is handed for a run that failed with `e`: `spawn_worker`'s
hold decision. A first-attempt lookup timeout that brings `lookup_streak`
(`streak`, its value before) to the limit or past it is handed over as an outage. -/
def heldErrW (retried : Bool) (streak : Nat) : Err → Err
  | .lookupTimeout =>
    if retried = false ∧ lookupTimeoutLimit ≤ streak + 1 then .unavailable else .lookupTimeout
  | e => e

/-- The worker holds a write exactly on an outage, or on a first-attempt lookup
timeout that brings the run of them to the limit or past it. -/
abbrev Spec.heldErrW_holds : Prop :=
  ∀ (retried : Bool) (streak : Nat) (e : Err),
    holdsS (heldErrW retried streak e) = true ↔
      (e = .unavailable ∨ (e = .lookupTimeout ∧ retried = false ∧ lookupTimeoutLimit ≤ streak + 1))

/-- **A single lookup timeout settles `failed` and consumes the intent** (below
the run limit, or a held write's retry). Rust test
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

/-- **A run of lookup timeouts holds the write**: a first attempt once the run
has reached the limit is held, as an outage is. Rust test
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

/-- **The serve applies only stored, unconsumed rows.** -/
abbrev Spec.serveR_applied_pending : Prop :=
  ∀ {live : Bool} {rout rout' : Job → RR} {s' : Streaks} {intents : List Intent} {x : Job},
    x ∈ (serveR live rout rout' s' intents).applied →
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none

/-- **No write is applied twice across a crash**: a write in a durable prefix of
the session's log has its consume beside it, so the next serve does not apply
it again (one process's log over an empty intent table). -/
abbrev Spec.crash_never_reappliesW : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore) (evs : List Ev) (all : List Job) (n : Nat)
    (r : Key) (live : Bool) (rout rout' : Job → RR) (s' : Streaks),
    Mut.write r ∈ ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten →
    ∀ x ∈ (serveR live rout rout' s' (fixedLoad (intentsOf
      ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten))).applied,
      x.key ≠ r

/-! ## Exactly one fate -/

/-- The five fates of an admitted write at close. -/
inductive FateW where
  | applied
  | failedConsumed
  | failedFenced
  | durable
  | unflushed
  deriving DecidableEq, Repr

/-- What each fate says of write `j` after `closeW flush now'` over `st`. -/
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

/-- **Every admitted write has exactly one fate at a close that reads no dead
letter** (`closeW`). -/
abbrev Spec.shutdown_exactly_oneW : Prop :=
  ∀ {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev} {st : WS},
    st = runW c sz now (WS.init (admitStore all)) evs →
    (st.admitted.map (·.key)).Nodup →
    (∀ j ∈ st.admitted, j ∈ all) →
    ∀ (flush : CloseFlush) (now' : Int) (j : Job),
    j ∈ st.admitted →
    (∃ f : FateW, f.holds all st flush now' j) ∧
      ∀ f g : FateW, f.holds all st flush now' j → g.holds all st flush now' j → f = g

/-! ## Dead-lettered batches

A close whose final flush completed sees a history: each batch of the
session's log committed or dead-lettered. -/

/-- How one batch of the session's log left the process before the close:
committed by a flush, or dropped as a dead letter (`FlushLoop::drain_and_flush`). -/
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

/-- The puts a dead letter dropped: what the flush records
(`Graph::note_dead_lettered_intents`). -/
def droppedPuts (log : List Batch) (hist : List Landed) : List Key :=
  keysLanded log hist .deadLettered

/-- The puts the store committed. -/
def committedPuts (log : List Batch) (hist : List Landed) : List Key :=
  keysLanded log hist .committed

/-- The session's whole log: admission's puts, then the workers' batches. -/
def sessLog (all : List Job) (st : WS) : List Batch := admitLog all ++ st.log

/-- The flush outcome receipt `k` is settled by: a write whose put a dead letter
dropped is settled as after an incomplete flush. -/
def flushFor (flush : CloseFlush) (dropped : List Key) (k : Key) : CloseFlush :=
  if k ∈ dropped then .incomplete else flush

/-- `settle_deferred`: each set-aside receipt by `flushFor`
(`Graph::intent_dead_lettered`). -/
def settleDeferredD (flush : CloseFlush) (dropped : List Key) (now : Int) (ids : List Key)
    (s : RStore) : RStore :=
  ids.foldl (fun s r => update s r (deferEntry (flushFor flush dropped r) now)) s

/-- `close()` with the dead-lettered puts: `closeW` with `settleDeferredD`. -/
def closeD (flush : CloseFlush) (dropped : List Key) (now : Int) (st : WS) : RStore :=
  settleDeferredD flush dropped now (deferredW st) st.store

/-- **With no dead letter, the close is `closeW`.** -/
abbrev Spec.closeD_agrees : Prop :=
  ∀ (flush : CloseFlush) (now : Int) (st : WS) (k : Key),
    ansOf (closeD flush [] now st) k = ansOf (closeW flush now st) k

/-- **No admitted write is lost silently at close**: each is applied, failed and
consumed, failed after the fence and owed, `intent_durable` and owed (its put
not dead-lettered), or `close_unflushed`. Rust test
`a_close_that_cannot_drain_defers_acked_writes_as_durable_intents`. -/
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

/-- **The close never answers `intent_durable` for a write whose put a dead letter
dropped**, whatever the flush outcome. Rust test
`a_close_with_an_empty_tail_does_not_vouch_for_a_dead_lettered_intent`. -/
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

/-- **Once every batch has landed, a write the close answers `intent_durable`
has its put committed.** -/
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

/-- **At a close whose flush completed, a set-aside write answers
`intent_durable` exactly when its put is committed**, else `close_unflushed`. -/
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

/-- **A retried close whose flush completes upgrades `close_unflushed` only for a
put not dead-lettered.** Rust test
`a_retried_close_upgrades_only_the_intents_it_can_vouch_for`. -/
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

/-! ## Worker panics and the dead-lettered set

A worker can panic between its commit and its settle, leaving a consumed
write unsettled for the close to set aside. -/

/-- A step that touches the dead-lettered set (`Graph::dead_lettered_intents`):
a write's consume (under the graph lock), its settle block, or a flush
dead-lettering a batch whose puts are `ks`. -/
inductive DEv where
  | consume (k : Key)
  | settle (k : Key)
  | deadLetter (ks : List Key)
  deriving DecidableEq, Repr

/-- The set (`dl`), the consumed-and-unsettled record (`cu`), and which writes
were consumed and settled and which puts were dropped. -/
structure DS where
  dl : List Key
  cu : List Key
  consumed : List Key
  settled : List Key
  dropped : List Key
  deriving DecidableEq, Repr

def DS.init : DS := ⟨[], [], [], [], []⟩

/-- The Rust's rule: a consume records the write (`Graph::mark_consumed_unsettled`),
its settle forgets it from both sets (`Graph::settled_write`), and the note
skips only a consumed write out of `cu`, that is, a settled one. -/
def stepP (s : DS) : DEv → DS
  | .consume k => { s with consumed := k :: s.consumed, cu := k :: s.cu }
  | .settle k =>
    { s with settled := k :: s.settled, cu := s.cu.filter (· ≠ k), dl := s.dl.filter (· ≠ k) }
  | .deadLetter ks =>
    { s with dropped := ks ++ s.dropped,
             dl := ks.filter (fun k => k ∉ s.consumed ∨ k ∈ s.cu) ++ s.dl }

/-- What a close whose final flush completed answers a set-aside receipt `k`
(one not settled), given the set. -/
def vouches (s : DS) (k : Key) : Bool := decide (k ∉ s.settled) && decide (k ∉ s.dl)

/-- A worker never consumes a write it already settled. -/
def DEv.ok (s : DS) : DEv → Prop
  | .consume k => k ∉ s.settled
  | _ => True

/-- Every step of a worker trace is one a worker can take. -/
def DOk : DS → List DEv → Prop
  | _, [] => True
  | s, e :: es => e.ok s ∧ DOk (stepP s e) es

/-- **The set never lets the close vouch for a dropped put**, panicked workers
included. Rust test
`a_worker_that_panics_after_its_commit_does_not_get_intent_durable`. -/
abbrev Spec.settlePrune_never_vouches_dropped : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∈ (evs.foldl stepP DS.init).dropped →
    vouches (evs.foldl stepP DS.init) k = false

/-- For a write not settled, the set holds it exactly when its put was dropped. -/
abbrev Spec.settlePrune_exact : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∉ (evs.foldl stepP DS.init).settled →
    (k ∈ (evs.foldl stepP DS.init).dl ↔ k ∈ (evs.foldl stepP DS.init).dropped)

/-- The consumed-and-unsettled record holds only writes consumed and not settled. -/
abbrev Spec.settlePrune_unsettled_bounded : Prop :=
  ∀ (evs : List DEv),
    DOk DS.init evs →
    ∀ (k : Key),
    k ∈ (evs.foldl stepP DS.init).cu →
    k ∈ (evs.foldl stepP DS.init).consumed ∧ k ∉ (evs.foldl stepP DS.init).settled

/-- A settled write leaves the set and the record, and a later dead letter of
its put does not note it. -/
abbrev Spec.settlePrune_settled_forgotten : Prop :=
  ∀ (s : DS) (k : Key),
    k ∈ s.consumed →
    ∀ (ks : List Key),
    k ∉ (stepP s (.settle k)).dl ∧ k ∉ (stepP s (.settle k)).cu ∧
      k ∉ (stepP (stepP s (.settle k)) (.deadLetter ks)).dl

/-! ## The lookup-timeout streak -/

/-- What moves `lookup_streak`, in order: a commit (`WorkerCommit`, which resets
it), the worker's `Ok` arm and settle block after it, and a first-attempt
lookup timeout. -/
inductive SEv where
  | commit
  | okArm
  | settleOk
  | timeout
  deriving DecidableEq, Repr

/-- `lookup_streak` as the worker counts it: reset at the commit, in its
critical section. -/
def streakNew (s : Nat) : SEv → Nat
  | .commit => 0
  | .okArm => s
  | .settleOk => s
  | .timeout => s + 1

/-- The documented count: the lookup timeouts since the last write applied. -/
def runSince (evs : List SEv) : Nat :=
  ((evs.reverse.takeWhile (fun e => e != .commit)).filter (fun e => e == .timeout)).length

/-- **A first-attempt lookup timeout is held exactly when it is at least the
limit-th since the last commit.** -/
abbrev Spec.heldNew_iff : Prop :=
  ∀ (evs : List SEv),
    holdsS (heldErrW false (evs.foldl streakNew 0) .lookupTimeout) = true ↔
      lookupTimeoutLimit ≤ runSince evs + 1

/-- **A first-attempt lookup timeout with none since the last commit settles
`failed`** and consumes its intent. Rust tests
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

end Lambo.Drain
