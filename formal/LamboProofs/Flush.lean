import LamboProofs.Lease

/-
# The store flush path: retry ladder, dead letters, fence and shutdown

Model of the write-behind flush loop in `src/store/flush.rs`: `FlushLoop::run`
(the top-of-loop fence check and the `biased` `select!`), `cycle` /
`drain_and_flush`, `flush_with_retry`, `requeue_pending`, `drop_pending`, the
constants `BACKOFF_BASE` (100 ms), `BACKOFF_CAP` (10 s) and `RETAINED_BACKOFF`
(= the cap), `FlushParams` (`retries`, `max_batch`, `log_max`) and
`FlushStats::dead_lettered`; `StoreError::is_retryable` in `src/types/mod.rs`;
the error classification in `src/store/error.rs` (`Cls`, #301 item 3); and
two clauses of the `GraphStore::flush` contract in `src/store/mod.rs`, the
fencing token and "Idempotency contract (flush replay)".

## Model

* `Store α := List α → Nat → Outcome`: the store is any function from (batch,
  attempt number) to an outcome. `Outcome.lostAck` is a commit the loop sees as
  a retryable `Backend` error (a timeout or a connection lost after `COMMIT`);
  `fail` commits nothing, since every adapter applies a batch in one
  transaction.
* `ladder` / `flushWithRetry` is `flush_with_retry`; `cycle` is
  `drain_and_flush` with its early returns in the Rust order (empty, degraded,
  over `log_max`, post-retry hold, early poll below `max_batch`), then the
  `match` on the ladder's result (`settle`).
* `iter` is one pass of the loop: the `select!` branch that fired, then the
  fence check that the Rust runs at the top of the next iteration, before it
  waits again. `step` adds graph writes and the heartbeat latching the fence;
  `run` folds a list of events.
* Ghost state: `hist` (every mutation ever appended to the graph log), `ledger`
  (every batch that left `pending`, with its fate) and `calls` (every
  `store.flush` call with its batch and outcome).
* Time is a natural number of milliseconds on the tokio clock, supplied with
  each wake-up. A ladder's duration is the sum of its sleeps (store calls take
  no time), so the model's post-retry deadline is at most the real one.

## Results

Retry ladder:
* `flushWithRetry_backoff`, `backoffAt_capped`, `flushWithRetry_sleep_total`:
  the sleeps never decrease and never exceed `BACKOFF_CAP` ("doubles per
  retry", "Cap on a single backoff sleep"); from the eighth on they equal it.
* `ladder_length_le`, `ladder_exhausted`: at most `retries + 1` calls, and
  exactly that many when the result is a retryable error ("total attempts =
  `retries` + 1"). `ladder_same_batch`: every call presents the same batch.
* `ladder_dropLast_retryable`, `stale_write_one_call`: only a retryable error
  is retried; `Constraint` and `StaleWrite` end the ladder at once ("bail out
  of the retry ladder").
* `ladder_ok_effect`, `replay_absorbs`: with the idempotent upsert the
  contract requires (`Idempotent`, shown to hold for upsert by key in
  `upsert_idempotent`), a batch whose ladder succeeds is applied once in
  effect, lost acknowledgements included, and a retained batch re-sent with
  newer mutations behind it lands as if sent once. Without the assumption the
  first fails (an append-only `decide` example). Both assume nothing else
  writes the store between the attempts (see `Idempotent`).

The loop, over every run from `St.init` (`good_reachable`):
* `Good.conserve`: ledger, then `pending`, then the graph log, is exactly the
  history. Nothing is lost or duplicated while the loop runs.
* `step_calls_unconsumed`, `retained_leads`, `committed_sublist`: every batch
  sent is the whole unconsumed tail of the history, oldest first, so a
  retained batch is always sent first and the store is written in log order
  ("drained mutations are appended *after* whatever is already pending").
* `run_calls_after`, `never_resent`: nothing that left `pending` (flushed,
  dead-lettered, dropped) is ever sent again; a dead letter is never retried.
* `transient_retried`, `exhausted_retained`: a retryable failure is retried
  with the same batch; after the last retry the batch is retained, not
  dropped or counted ("Never dropped (except dead letters)").
* `constraint_dead_lettered`, `Good.dead`: a constraint violation costs one
  call, sends the whole batch to the ledger as a dead letter and adds exactly
  one to `dead_lettered` ("never retried, never retained"). Since #345 the
  arm also notes the receipt ids of the write intents the batch put
  (`Graph::note_dead_lettered_intents`), a graph-side record this model does
  not carry; what the close answers for them is `Drain.closeD`.
* `cycle_held`, `cycle_poll_waits`: no call during the post-retry hold, nor on
  a poll below `max_batch`.
* `degraded_silent`, `run_stopped_silent`, `fenced_stops`: a degraded session
  never calls the store again; a fenced loop stops at its next wake-up and
  then never calls it again.
* `stop_requeues`, `stopped_accounted`, `no_silent_loss`: after the loop stops,
  the history is the ledger followed by the graph log, so every mutation was
  committed, dead-lettered (counted), dropped by a degraded or fenced session
  (flag set), or is back at the front of the graph log for `Memory::close`.
  `abort_loses`: the one way out of this is the documented hard `abort`.
* `fencing_refuses_after_takeover`, `flush_after_takeover` (with
  `Lease.stale_token_refused`, #1): once another identity holds the lease,
  every attempt with the loop's token is refused as `StaleWrite` and commits
  nothing.

## What the model shows about the Rust before #293

The definitions `settle`, `cycle`, `iter`, `step`, `run` and the theorems
above are the loop as it was before #293. The first three results below pin
the behaviour that issue fixes (see "The fix (#293)" next); the fourth is
unchanged by it.

* `stale_write_retained`: `is_retryable` says the loop "treats it like a
  constraint"; it does not. Only the `Constraint` arm dead-letters, so a
  `StaleWrite` (not erased) is retained with the post-retry hold and
  `retained_resent` sends it again every `RETAINED_BACKOFF` (logged as
  `BackendFlushFailed ... never dropped`) for as long as the fence stays
  unlatched. Normally the heartbeat (every `LEASE_HEARTBEAT_INTERVAL`, 15 s)
  sees `Held` and latches it after one or two resends. It never latches if
  the holder's own refresh re-takes the lease: a refresh is an acquire
  (`GraphStore::refresh_lease`), an expired row held by another identity is
  taken over with a new token (`MemoryStore::acquire_lease`, the SQL upsert),
  and the heartbeat ignores the token an `Acquired` returns. After the
  documented `OPERATOR_OVERRIDE` (holder `lambo:released`, row expired), or a
  takeover and release by another writer within one heartbeat interval, the
  wedged holder's next refresh wins at `token + 1`
  (`override_refresh_retakes`). Its flush loop keeps the old token, so every
  flush is refused, retained and resent every 10 s, the fence never latches,
  the handle keeps accepting writes until the session degrades, and other
  writers get `Held` (the `exRetaken` examples).
* `flush_after_fence_latched`: the fence check runs before the loop waits, so
  a fence latched during the wait is seen only after the branch that ends the
  wait has run. Since the wait is at most one `POLL_QUANTUM` (100 ms), that is
  a further flush only when the fence latches within 100 ms before a due
  tick, or while `pending` holds at least `max_batch` mutations. Then the
  comment "once the heartbeat latches a lost lease no further flush is
  issued" does not hold; the store's token check is what refuses that
  flush.
* `stop_requeues`: a stop that ends the wait after the fence latched hands the
  tail back to the graph log instead of dropping it, unlike the comment's
  "writing them (here OR handing them back for `close`'s final flush)".
  `Memory::close` checks `lease_lost` itself before it stops the loop, so in
  practice this only changes which error a racing close reports.
* `constraint_dead_lettered`: the dead letter is the whole merged `pending`,
  so mutations retained from an earlier outage are dropped with a later
  write's constraint violation, and `dead_lettered` counts one (see the
  `exPoison` example).

## The fix (#293)

Beside the definitions above, `settleFixed`, `cycleFixed`, `iterFixed`,
`stepFixed` and `runFixed` model the loop after #293, and `Lease.beatLatches`
the fixed heartbeat (`Lease.beatLatchesOld` is the one before). Three changes:
a `StaleWrite` latches the fence (as the erased arm already did); the fence is
re-checked right after the drain, before anything can reach the store; and the
stop branch drops a fenced tail. `cycleFixed_eq` shows the fixed cycle is the
old one with exactly those substitutions.

* `takeover_fences_fixed`, `override_fences_fixed` (with
  `Lease.beat_latches_iff`, `Lease.override_beat_latches`): after any takeover,
  the operator-override re-take included, the fixed heartbeat latches at its
  next beat, within one `LEASE_HEARTBEAT_INTERVAL`, where the old one misses
  the re-take; and from the latch on the fixed loop never calls the store
  (`no_flush_after_fence_fixed`, `runFixed_fenced_silent`).
* `fence_before_due_tick_fixed`: the scenario of `flush_after_fence_latched`
  makes no call; the drained tail is a fenced drop.
* `stale_write_fences_fixed`, `stale_write_never_resent`,
  `stale_write_dropped_fixed`: any event whose calls include a `StaleWrite`
  leaves the loop fenced and stopped, and no later event calls the store, so
  a stale write is never retained or re-sent (contrast
  `stale_write_retained`).
* `stop_drops_fenced`: a stop after the fence drops the tail instead of
  handing it back (contrast `stop_requeues`).
* `good_reachable_fixed`, `no_silent_loss_fixed`, `stopped_accounted_fixed`:
  the invariants still hold for the loop; a stale-write drop is a fenced drop,
  made only once the fence is latched.
* `closeOk`, `close_ok_no_drop`, `close_ok_over_drop`: `Memory::close` over the
  fixed loop. The fence can now latch while close stops the loop (a flush in
  flight comes back stale), so a close that read the fence only before the
  stop reports `Ok` over the dropped batch (`close_ok_over_drop`, found in
  review of the fix). A close that reads it again after the join returns `Ok`
  only when every batch that left `pending` was committed or dead-lettered
  (`close_ok_no_drop`). The Rust close re-checks after joining the loop.
* `stepFixed_agrees`, `Lease.beat_own_generation`: away from a latched fence
  and a stale write, and for a heartbeat on the holder's own generation, the
  fix changes nothing.

The fixed Rust also checks the fence before each retry in `flush_with_retry`,
so a fence latched during a backoff sleep stops the ladder. The model latches
the fence only between wake-ups, so that check is not modelled. The fixed
heartbeat also stops beating once it latches, on `Held` or on a re-take, so a
lease it re-took lapses at its TTL and a fenced handle cannot re-take one
later; the model does not follow the lease row past the latch.

## The two unguarded store calls (#301)

The loop above takes `erased` as an input and leaves `publish_stats` out, but
both are store calls: `erased()` reads the lease row (`refused_as_erased` →
`read_lease`) after a ladder that ended in an error other than a constraint
violation (`readsLease`), and `publish_stats` calls `write_flush_stats`.
Before #301 neither ran under `FLUSH_ATTEMPT_TIMEOUT` or `CatchUnwindPoll`.
`PEv` makes each a `Probe` that returns, panics or never returns, and
`stepUnguarded` / `runUnguarded` is the #293 loop with them unguarded (`Task`
adds the "parked for good" state).

* `lease_panic_loses`, `stats_panic_loses`: a panic in either kills the task
  holding a non-empty `pending` (`abort`): no ledger entry, no count,
  `degraded()` and the fence false, and the history no longer accounted for.
  `close_ok_over_panic`: `close` logs the join error, finds an empty log and
  returns `Ok`.
* `lease_hang_blocks_stop`, `hung_stays`: a read that never returns parks the
  task holding the batch; no later event, a stop included, gets it out, so
  `close`'s join never returns (`closeOkUnguarded` is `none`).

The fix: both calls run under the flush attempt's timeout and catch-unwind. A
lease read that panics or times out counts as a failed read
(`Tombstone::Unknown`, not erased, `guardLease`), so the batch takes the
stale-write or retain arm as before; a stats write that panics or times out is
ignored as a failed one was. The guarded loop (`PEv.guard`, `runGuarded`) is
the #293 loop with that `erased` input, so the `Fixed` theorems carry over:
`good_reachable_guarded`, `no_silent_loss_guarded`,
`stopped_accounted_guarded`, `close_ok_no_drop_guarded`, and
`close_ok_accounted_guarded` (a close that returns `Ok` after the loop exited
leaves every mutation committed, dead-lettered or in the final flush).
`stop_exits_guarded`: a stop always ends the loop. `stepUnguarded_agrees`,
`closeOkUnguarded_agrees`, `runGuarded_lift`: where the calls return, the fix
changes nothing. `cycleFixed_erased_irrelevant`: a cycle that does not read
the lease does not depend on the `erased` input, so the `false` passed there
is arbitrary.

What the model does not show: it takes the erased answer `er` as an input,
so it proves only that "not erased" after a failed read loses nothing and
blocks nothing, not that the answer is right. That a wrong "not erased" never
writes into an erased session rests on the adapters' commit-time fence,
`erase::check_fence`, which refuses an erasure tombstone whatever token is
presented, inside the flush transaction: `MemoryStore` (`store/memory.rs`),
SQLite (`store/sqlite/persistence.rs`) and Postgres / CockroachDB
(`store/pg/persistence.rs`). That refusal is a `StaleWrite`, so after a
"not erased" answer the loop takes the stale-write arm and fences itself all
the same; only the erased latch's early wake-up waits for the heartbeat.
`stop_exits_guarded` assumes what the
Rust timeout provides: a guarded call that never returns is cut off at
`FLUSH_ATTEMPT_TIMEOUT` (the model's calls take no time), so the cycle ends
and the stop is served.

## Error classes (#301 item 3)

`Cls` is how the adapters classify a failed flush (`ErrorClass` and
`map_write_err` in `src/store/error.rs`): retryable, constraint, stale, and
permanent but not a constraint (SQLSTATE `42P01`, say). `CStore.via m` is the
store as the loop sees it when the adapters map a class through `m`.

* `permanent_retryable_old`, `permanent_retried_retained`: before the fix a
  permanent error reaches the loop as `Backend` (`Cls.errOld`), so the loop as
  it is now retries it `retries` times and retains the batch, to resend it
  after every hold until the backlog degrades the session.
* The fix: `Err.permanent` (`StoreError::Permanent`, `Cls.errFixed`), not
  retryable, and the `drain_and_flush` arm `settleClassed`, which degrades the
  session (after the erased arm). `cycleClassed`, `iterClassed`,
  `stepClassed`, `runClassed` and `runClassedGuarded` are the loop after it.
  `permanent_one_call`, `permanent_degrades_classed`,
  `permanent_never_resent_classed`: a permanent error costs one call, degrades
  the session with the batch held for the degraded drop, and nothing calls the
  store again. `close_not_ok_degraded`: `close` then reports an error.
* `transient_retryable_fixed`, `via_fixed_agrees`, `stepClassed_agrees`,
  `runClassed_old`: every other class is handled as before.
* `good_reachable_classed`, `no_silent_loss_classed`,
  `stopped_accounted_classed`, `close_ok_no_drop_classed`,
  `close_ok_accounted_classed`: the invariants and the close theorems hold of
  the fixed loop, over the guarded events of #301 item 2.
* Degrade, not dead-letter: the `exGone` examples run the other candidate
  (`Cls.errDeadLetter`, a permanent error dead-lettered like a constraint
  violation) against a store whose table is gone. It drops every batch and
  `close` returns `Ok` with nothing committed. Degrading keeps the writes in
  the graph and makes `close` an error. `exBadValue` was the cost: a
  permanent error that belongs to one batch ended durability for the session,
  where dead-lettering would drop that batch alone.
* So the permanent class is split by scope (`Cause`, after review): a value
  the store rejects (SQLSTATE `21`, `22`, `44`, `54000`; `SQLITE_TOOBIG`,
  `SQLITE_MISMATCH`) reaches the loop as `StoreError::Constraint`
  (`Cause.errSplit`) and is dead-lettered with one call while the session
  goes on (`value_rejected_dead_letters`, the `exBadValue` example); an error
  about the store itself degrades as before (`store_permanent_degrades_split`,
  `exGoneK`). The loop is unchanged; `via_split_agrees`,
  `no_silent_loss_split` and `close_ok_accounted_split` carry the results
  over.

## Left out

Read-access drains and their budget (#30); the `mutation_epoch` and GC-mark
carry (#17, #29); the graph's unflushed set (`mark_durable_through`,
`pin_unflushed`, #60); `lag`, `depth` and what `publish_stats` writes (only
whether its call returns is modelled, in the #301 section); writes that land
during a cycle (they land between iterations here); the `select!` priority
(the event list picks the branch); what `refused_as_erased` reads (an input,
`erased`; whether the read returns is a probe in the #301 section), and the
time a guarded call spends before it times out; `Memory::close`'s own final
flush, except as the `closeOk` outcome below. The model also drops an
ordering the Rust enforces: `select!` re-arms `sleep(POLL_QUANTUM)` on every
iteration, so the real loop wakes, and runs the fence check, at least every
100 ms. Here a run may space its wake-ups arbitrarily, so some event lists
(a fence latched long before the next wake-up) are not realizable; the
examples below include the polls that make them so. The theorems hold of
every run, realizable or not.
-/

namespace Lambo.Flush

/-! ## Errors and attempt outcomes -/

/-- `StoreError`'s variants. A timed-out attempt (`FLUSH_ATTEMPT_TIMEOUT`) and
a panicking backend (`CatchUnwindPoll`) both reach the ladder as `Backend`.
`permanent` is `StoreError::Permanent`, added by the fix for #301 item 3:
before it, no adapter produced it, and a permanent error reached the loop as
`backend` (`Cls.errOld`). -/
inductive Err where
  | sessionNotFound
  | notFound
  | capability
  | invariant
  | backend
  | constraint
  | staleWrite
  | other
  | permanent
  deriving DecidableEq, Repr

/-- `StoreError::is_retryable`: everything except `Constraint`, `StaleWrite`
and (since #301) `Permanent`. Before #301 the code had no `Permanent`, so on
the errors that existed then this is the old function. -/
def Err.retryable : Err → Bool
  | .constraint => false
  | .staleWrite => false
  | .permanent => false
  | _ => true

/-- What one `store.flush` attempt did. Every adapter applies a batch in one
transaction (the SQL adapters' `persistence.rs`, `MemoryStore`'s working
copy), so a reported failure committed nothing, except when the
acknowledgement itself is lost: `lostAck` is a commit that the loop sees as a
retryable `Backend` error (the attempt timed out, or the connection dropped
after `COMMIT`). -/
inductive Outcome where
  | ok
  | fail (e : Err)
  | lostAck
  deriving DecidableEq, Repr

def Outcome.reported : Outcome → Option Err
  | .ok => none
  | .fail e => some e
  | .lostAck => some .backend

def Outcome.committed : Outcome → Bool
  | .ok => true
  | .fail _ => false
  | .lostAck => true

/-- The store, abstractly: the outcome of presenting batch `b` as the `n`-th
`store.flush` call of the task's life. Any function will do. -/
abbrev Store (α : Type) := List α → Nat → Outcome

/-! ## Backoff

Durations are milliseconds. -/

/-- `BACKOFF_BASE`. Pinned by `formal_constants_tests::flush`. -/
def backoffBase : Nat := 100
/-- `BACKOFF_CAP`. Pinned by `formal_constants_tests::flush`. -/
def backoffCap : Nat := 10000
/-- `RETAINED_BACKOFF = BACKOFF_CAP`. Pinned by `formal_constants_tests::flush`. -/
def retainedBackoff : Nat := backoffCap

/-- The doubling, `next_backoff`: `(backoff * 2).min(BACKOFF_CAP)`. Pinned by
`formal_constants_tests::flush`, which evaluates it against `next_backoff` up
the ladder and past the cap. -/
def nextBackoff (b : Nat) : Nat := min (b * 2) backoffCap

def backoffAt : Nat → Nat
  | 0 => backoffBase
  | i + 1 => nextBackoff (backoffAt i)

theorem nextBackoff_le_cap (b : Nat) : nextBackoff b ≤ backoffCap := Nat.min_le_right _ _

theorem le_nextBackoff {b : Nat} (h : b ≤ backoffCap) : b ≤ nextBackoff b := by
  unfold nextBackoff
  exact Nat.le_min.mpr ⟨by omega, h⟩

theorem backoffAt_le_cap (i : Nat) : backoffAt i ≤ backoffCap := by
  cases i with
  | zero => decide
  | succ i => exact nextBackoff_le_cap _

theorem backoffAt_le_succ (i : Nat) : backoffAt i ≤ backoffAt (i + 1) :=
  le_nextBackoff (backoffAt_le_cap i)

theorem backoffAt_mono {i j : Nat} (h : i ≤ j) : backoffAt i ≤ backoffAt j := by
  induction j with
  | zero =>
    have : i = 0 := by omega
    subst this
    exact Nat.le_refl _
  | succ j ih =>
    by_cases hij : i = j + 1
    · subst hij
      exact Nat.le_refl _
    · exact Nat.le_trans (ih (by omega)) (backoffAt_le_succ j)

theorem backoffAt_seven : backoffAt 7 = backoffCap := by decide

theorem backoffAt_capped {i : Nat} (h : 7 ≤ i) : backoffAt i = backoffCap := by
  induction i with
  | zero => omega
  | succ i ih =>
    by_cases h7 : i + 1 = 7
    · rw [h7]
      exact backoffAt_seven
    · show nextBackoff (backoffAt i) = backoffCap
      rw [ih (by omega)]
      decide

example : (List.range 9).map backoffAt = [100, 200, 400, 800, 1600, 3200, 6400, 10000, 10000] := by
  decide


/-! ## The retry ladder -/

/-- One `flush_with_retry`: the calls it made, the backoff sleeps between
them, and its result (`none` is `Ok`). -/
structure Run (α : Type) where
  calls : List (List α × Outcome)
  sleeps : List Nat
  result : Option Err

section Ladder

variable {α : Type}

/-- `flush_with_retry` from attempt number `n`, current backoff `bo` and `k`
retries left: a non-retryable error returns at once; a retryable one is
retried after sleeping `bo` while retries remain, and the backoff becomes
`min(2 bo, BACKOFF_CAP)`. The batch is the same on every call (`pending` is
only borrowed). -/
def ladder (st : Store α) (b : List α) : Nat → Nat → Nat → Run α
  | n, _, 0 =>
    let o := st b n
    ⟨[(b, o)], [], o.reported⟩
  | n, bo, k + 1 =>
    let o := st b n
    match o.reported with
    | none => ⟨[(b, o)], [], none⟩
    | some e =>
      if e.retryable then
        let r := ladder st b (n + 1) (nextBackoff bo) k
        ⟨(b, o) :: r.calls, bo :: r.sleeps, r.result⟩
      else ⟨[(b, o)], [], some e⟩

/-- The ladder as the loop runs it: from `BACKOFF_BASE`, `retries` retries. -/
def flushWithRetry (st : Store α) (b : List α) (n retries : Nat) : Run α :=
  ladder st b n backoffBase retries

variable {st : Store α} {b : List α}

theorem ladder_ok {n bo k : Nat} (h : (st b n).reported = none) :
    ladder st b n bo k = ⟨[(b, st b n)], [], none⟩ := by
  cases k with
  | zero => simp [ladder, h]
  | succ k => simp [ladder, h]

theorem ladder_nonretryable {n bo k : Nat} {e : Err} (h : (st b n).reported = some e)
    (hr : e.retryable = false) :
    ladder st b n bo k = ⟨[(b, st b n)], [], some e⟩ := by
  cases k with
  | zero => simp [ladder, h]
  | succ k => simp [ladder, h, hr]

theorem ladder_last {n bo : Nat} :
    ladder st b n bo 0 = ⟨[(b, st b n)], [], (st b n).reported⟩ := by
  simp [ladder]

theorem ladder_retry {n bo k : Nat} {e : Err} (h : (st b n).reported = some e)
    (hr : e.retryable = true) :
    ladder st b n bo (k + 1) =
      ⟨(b, st b n) :: (ladder st b (n + 1) (nextBackoff bo) k).calls,
       bo :: (ladder st b (n + 1) (nextBackoff bo) k).sleeps,
       (ladder st b (n + 1) (nextBackoff bo) k).result⟩ := by
  simp [ladder, h, hr]

theorem ladder_same_batch (n bo k : Nat) : ∀ c ∈ (ladder st b n bo k).calls, c.1 = b := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp
      | true =>
        rw [ladder_retry h hr]
        intro c hc
        cases hc with
        | head => rfl
        | tail _ hc => exact ih _ _ c hc

theorem ladder_length_pos (n bo k : Nat) : 0 < (ladder st b n bo k).calls.length := by
  cases k with
  | zero => simp [ladder]
  | succ k =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp
      | true => rw [ladder_retry h hr]; simp

theorem ladder_length_le (n bo k : Nat) : (ladder st b n bo k).calls.length ≤ k + 1 := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp
      | true =>
        rw [ladder_retry h hr]
        have := ih (n + 1) (nextBackoff bo)
        simp only [List.length_cons]
        omega

theorem ladder_sleeps_length (n bo k : Nat) :
    (ladder st b n bo k).sleeps.length + 1 = (ladder st b n bo k).calls.length := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp
      | true =>
        rw [ladder_retry h hr]
        have := ih (n + 1) (nextBackoff bo)
        simp only [List.length_cons]
        omega

/-- The calls are numbered consecutively from `n`: call `i` is attempt `n + i`. -/
theorem ladder_attempt_index (n bo k : Nat) :
    ∀ i (hi : i < (ladder st b n bo k).calls.length),
      ((ladder st b n bo k).calls[i]'hi).2 = st b (n + i) := by
  induction k generalizing n bo with
  | zero => intro i hi; simp [ladder] at hi ⊢; subst hi; rfl
  | succ k ih =>
    cases h : (st b n).reported with
    | none => intro i hi; simp [ladder_ok h] at hi ⊢; subst hi; rfl
    | some e =>
      cases hr : e.retryable with
      | false => intro i hi; simp [ladder_nonretryable h hr] at hi ⊢; subst hi; rfl
      | true =>
        intro i hi
        simp only [ladder_retry h hr] at hi ⊢
        cases i with
        | zero => rfl
        | succ i =>
          simp only [List.getElem_cons_succ]
          rw [ih (n + 1) (nextBackoff bo) i]
          congr 1
          omega

/-- Every call but the last reported a retryable error: the ladder goes on
only past a retryable failure. -/
theorem ladder_dropLast_retryable (n bo k : Nat) :
    ∀ c ∈ (ladder st b n bo k).calls.dropLast,
      ∃ e, c.2.reported = some e ∧ e.retryable = true := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp
      | true =>
        rw [ladder_retry h hr]
        have hne : (ladder st b (n + 1) (nextBackoff bo) k).calls ≠ [] :=
          List.ne_nil_of_length_pos (ladder_length_pos _ _ _)
        rw [List.dropLast_cons_of_ne_nil hne]
        intro c hc
        cases hc with
        | head => exact ⟨e, h, hr⟩
        | tail _ hc => exact ih _ _ c hc

/-- The result is what the last call reported. -/
theorem ladder_result_last (n bo k : Nat) :
    ∃ c, (ladder st b n bo k).calls.getLast? = some c ∧
      (ladder st b n bo k).result = c.2.reported := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp [h]
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp [h]
      | true =>
        rw [ladder_retry h hr]
        obtain ⟨c, hc, hres⟩ := ih (n + 1) (nextBackoff bo)
        have hne : (ladder st b (n + 1) (nextBackoff bo) k).calls ≠ [] :=
          List.ne_nil_of_length_pos (ladder_length_pos _ _ _)
        refine ⟨c, ?_, hres⟩
        rw [List.getLast?_cons, hc]
        rfl

/-- A retryable error as the result means every retry was used. -/
theorem ladder_exhausted (n bo k : Nat) {e : Err} (h : (ladder st b n bo k).result = some e)
    (hr : e.retryable = true) : (ladder st b n bo k).calls.length = k + 1 := by
  induction k generalizing n bo with
  | zero => simp [ladder]
  | succ k ih =>
    cases h0 : (st b n).reported with
    | none => rw [ladder_ok h0] at h; simp at h
    | some e0 =>
      cases hr0 : e0.retryable with
      | false =>
        rw [ladder_nonretryable h0 hr0] at h
        simp at h
        subst h
        rw [hr] at hr0
        cases hr0
      | true =>
        rw [ladder_retry h0 hr0] at h ⊢
        simp only [List.length_cons]
        rw [ih (n + 1) (nextBackoff bo) h]

/-- The ladder's sleeps from backoff index `i`: `backoffAt i`, `backoffAt (i+1)`, ... -/
def sleepsFrom : Nat → Nat → List Nat
  | _, 0 => []
  | i, m + 1 => backoffAt i :: sleepsFrom (i + 1) m

theorem ladder_sleeps (n i k : Nat) :
    (ladder st b n (backoffAt i) k).sleeps =
      sleepsFrom i (ladder st b n (backoffAt i) k).sleeps.length := by
  induction k generalizing n i with
  | zero => simp [ladder, sleepsFrom]
  | succ k ih =>
    cases h : (st b n).reported with
    | none => rw [ladder_ok h]; simp [sleepsFrom]
    | some e =>
      cases hr : e.retryable with
      | false => rw [ladder_nonretryable h hr]; simp [sleepsFrom]
      | true =>
        rw [ladder_retry h hr]
        simp only [List.length_cons, sleepsFrom]
        have := ih (n + 1) (i + 1)
        simp only [backoffAt] at this
        rw [← this]

theorem mem_sleepsFrom {i m x : Nat} (h : x ∈ sleepsFrom i m) : ∃ j, i ≤ j ∧ x = backoffAt j := by
  induction m generalizing i with
  | zero => simp [sleepsFrom] at h
  | succ m ih =>
    simp only [sleepsFrom, List.mem_cons] at h
    cases h with
    | inl h => exact ⟨i, Nat.le_refl _, h⟩
    | inr h =>
      obtain ⟨j, hj, hx⟩ := ih h
      exact ⟨j, by omega, hx⟩

theorem sleepsFrom_le_cap {i m : Nat} : ∀ x ∈ sleepsFrom i m, x ≤ backoffCap := by
  intro x hx
  obtain ⟨j, _, rfl⟩ := mem_sleepsFrom hx
  exact backoffAt_le_cap j

theorem sleepsFrom_sorted (i m : Nat) : (sleepsFrom i m).Pairwise (· ≤ ·) := by
  induction m generalizing i with
  | zero => simp [sleepsFrom]
  | succ m ih =>
    simp only [sleepsFrom, List.pairwise_cons]
    refine ⟨?_, ih (i + 1)⟩
    intro x hx
    obtain ⟨j, hj, rfl⟩ := mem_sleepsFrom hx
    exact backoffAt_mono (by omega)

/-- **Backoff is monotone and capped.** The sleeps of a flush attempt's
retry ladder never decrease, and each is at most `BACKOFF_CAP` (10 s). -/
theorem flushWithRetry_backoff (n retries : Nat) :
    (flushWithRetry st b n retries).sleeps.Pairwise (· ≤ ·) ∧
    ∀ x ∈ (flushWithRetry st b n retries).sleeps, x ≤ backoffCap := by
  have e := ladder_sleeps (st := st) (b := b) n 0 retries
  unfold flushWithRetry
  rw [show backoffBase = backoffAt 0 from rfl, e]
  exact ⟨sleepsFrom_sorted _ _, sleepsFrom_le_cap⟩

/-- The total time a ladder sleeps is at most `retries * BACKOFF_CAP`. -/
theorem flushWithRetry_sleep_total (n retries : Nat) :
    (flushWithRetry st b n retries).sleeps.sum ≤ retries * backoffCap := by
  have hlen := ladder_sleeps_length (st := st) (b := b) n backoffBase retries
  have hle := ladder_length_le (st := st) (b := b) n backoffBase retries
  have hcap := (flushWithRetry_backoff (st := st) (b := b) n retries).2
  unfold flushWithRetry at hcap ⊢
  generalize (ladder st b n backoffBase retries).sleeps = l at hlen hcap ⊢
  have hl : l.length ≤ retries := by omega
  clear hlen hle
  induction l generalizing retries with
  | nil => simp
  | cons x l ih =>
    simp only [List.sum_cons]
    have hx := hcap x (List.mem_cons_self ..)
    have := ih (retries - 1) (fun y hy => hcap y (List.mem_cons_of_mem _ hy))
      (by simp at hl; omega)
    have h2 : (retries - 1) * backoffCap + backoffCap ≤ retries * backoffCap := by
      cases retries with
      | zero => simp at hl
      | succ r => simp [Nat.succ_mul]
    omega

end Ladder

/-! ## What the store holds: replay and idempotent upsert -/

section Effect

variable {α σ : Type}

/-- Apply a batch, one mutation at a time, in order. -/
def applyBatch (ap : σ → α → σ) (s : σ) (b : List α) : σ := b.foldl ap s

/-- The store state after a sequence of calls: a call changes the store
exactly when its transaction committed (`ok`, or a lost acknowledgement). -/
def effect (ap : σ → α → σ) (s : σ) (calls : List (List α × Outcome)) : σ :=
  calls.foldl (fun s c => if c.2.committed then applyBatch ap s c.1 else s) s

/-- **The assumption the code makes**: applying a batch a second time right
after it was applied changes nothing. It is implied by the `GraphStore::flush`
"Idempotency contract (flush replay)". The contract also covers replaying a
batch that partially succeeded; the model excludes partial success by the
one-transaction-per-batch reading of `Outcome`. `ladder_ok_effect` and
`replay_absorbs` also assume nothing else writes the store between the
attempts, which the Rust does not guarantee: the canonization task and
`write_flush_stats` write the same store concurrently. -/
def Idempotent (ap : σ → α → σ) : Prop :=
  ∀ s b, applyBatch ap (applyBatch ap s b) b = applyBatch ap s b

theorem applyBatch_append (ap : σ → α → σ) (s : σ) (b c : List α) :
    applyBatch ap s (b ++ c) = applyBatch ap (applyBatch ap s b) c := by
  simp [applyBatch, List.foldl_append]

/-- A replayed prefix is absorbed: when the store already holds `b` (a lost
acknowledgement) and the retained batch `b ++ c` is sent again, the result is
as if `b ++ c` had been applied once. -/
theorem replay_absorbs {ap : σ → α → σ} (hI : Idempotent ap) (s : σ) (b c : List α) :
    applyBatch ap (applyBatch ap s b) (b ++ c) = applyBatch ap s (b ++ c) := by
  rw [applyBatch_append, hI, ← applyBatch_append]

/-- Calls that all present the same batch leave the store either untouched
or holding the batch applied once, however many of them committed. -/
theorem effect_same_batch {ap : σ → α → σ} (hI : Idempotent ap) (b : List α) :
    ∀ (calls : List (List α × Outcome)) (s : σ), (∀ c ∈ calls, c.1 = b) →
      (effect ap s calls = s ∧ ∀ c ∈ calls, c.2.committed = false) ∨
      effect ap s calls = applyBatch ap s b
  | [], s, _ => Or.inl ⟨rfl, by simp⟩
  | c :: cs, s, h => by
    have hc : c.1 = b := h c (List.mem_cons_self ..)
    have hcs : ∀ c ∈ cs, c.1 = b := fun c' hc' => h c' (List.mem_cons_of_mem _ hc')
    cases hcom : c.2.committed with
    | false =>
      have e : effect ap s (c :: cs) = effect ap s cs := by
        simp [effect, List.foldl_cons, hcom]
      rw [e]
      cases effect_same_batch hI b cs s hcs with
      | inl h' =>
        left
        refine ⟨h'.1, ?_⟩
        intro c' hc'
        cases hc' with
        | head => exact hcom
        | tail _ hc' => exact h'.2 c' hc'
      | inr h' => exact Or.inr h'
    | true =>
      right
      have e : effect ap s (c :: cs) = effect ap (applyBatch ap s b) cs := by
        simp [effect, List.foldl_cons, hcom, hc]
      rw [e]
      cases effect_same_batch hI b cs (applyBatch ap s b) hcs with
      | inl h' => exact h'.1
      | inr h' => rw [h', hI]

/-- **A retried batch lands exactly once.** If the ladder ends in `Ok`, the
store holds the batch applied once, even when earlier attempts committed but
lost their acknowledgement (a timeout or a connection lost after `COMMIT`). -/
theorem ladder_ok_effect {ap : σ → α → σ} (hI : Idempotent ap) {st : Store α} {b : List α}
    {n bo k : Nat} (hres : (ladder st b n bo k).result = none) (s : σ) :
    effect ap s (ladder st b n bo k).calls = applyBatch ap s b := by
  cases effect_same_batch hI b _ s (ladder_same_batch n bo k) with
  | inr h => exact h
  | inl h =>
    obtain ⟨c, hc, hr⟩ := ladder_result_last (st := st) (b := b) n bo k
    rw [hres] at hr
    have hmem : c ∈ (ladder st b n bo k).calls := List.mem_of_getLast? hc
    have hcom := h.2 c hmem
    cases ho : c.2 with
    | ok => rw [ho] at hcom; cases hcom
    | fail e => rw [ho] at hr; cases hr
    | lostAck => rw [ho] at hr; cases hr

/-! ### The assumption holds for upsert by natural key

Every Lambo mutation kind is an upsert or a delete keyed by its natural key
(the contract's own words). As a store that is a map from keys to an optional
value, last write wins. -/

/-- One upsert (`some v`) or delete (`none`) on key `k`. -/
def upsert {K V : Type} [DecidableEq K] (s : K → Option V) (m : K × Option V) : K → Option V :=
  fun k => if k = m.1 then m.2 else s k

/-- The last write to `k` in `b`, if any. -/
def lastWrite {K V : Type} [DecidableEq K] (k : K) : List (K × Option V) → Option (Option V)
  | [] => none
  | m :: b =>
    match lastWrite k b with
    | some v => some v
    | none => if k = m.1 then some m.2 else none

theorem applyBatch_upsert {K V : Type} [DecidableEq K] (b : List (K × Option V)) :
    ∀ (s : K → Option V) (k : K),
      applyBatch upsert s b k = (match lastWrite k b with | some v => v | none => s k) := by
  induction b with
  | nil => intro s k; rfl
  | cons m b ih =>
    intro s k
    show applyBatch upsert (upsert s m) b k = _
    rw [ih]
    simp only [lastWrite]
    cases lastWrite k b with
    | some v => rfl
    | none =>
      simp only [upsert]
      by_cases hk : k = m.1 <;> simp [hk]

theorem upsert_idempotent {K V : Type} [DecidableEq K] :
    Idempotent (upsert (K := K) (V := V)) := by
  intro s b
  funext k
  rw [applyBatch_upsert, applyBatch_upsert]
  cases lastWrite k b with
  | some v => rfl
  | none => rfl

/-- Without the assumption the guarantee fails: an append-only store (plain
`INSERT`, which the contract forbids) holds a batch twice after a lost
acknowledgement and a retry. -/
example :
    effect (fun (s : List Nat) m => s ++ [m]) []
      (ladder (fun _ n => if n = 0 then .lostAck else .ok) [7] 0 backoffBase 3).calls
      = [7, 7] := by
  decide

/-- With the upsert store the same history leaves key `7` written once. -/
example :
    effect upsert (fun _ => none)
      (ladder (fun _ n => if n = 0 then .lostAck else .ok) [((7 : Nat), some (1 : Nat))] 0
        backoffBase 3).calls 7 = some 1 := by
  decide

end Effect

/-! ## The flush loop -/

/-- Where a batch that left `pending` went. -/
inductive Fate where
  | committed
  | deadLettered
  | droppedDegraded
  | droppedFenced
  deriving DecidableEq, Repr

/-- `FlushParams` without `interval` (time enters through the wake-ups). -/
structure Params where
  retries : Nat
  maxBatch : Nat
  logMax : Nat

/-- The loop's state, the graph log it drains, and ghost fields for the
proofs. -/
structure St (α : Type) where
  /-- Ghost: every mutation ever appended to the graph log, in order. -/
  hist : List α
  /-- The graph's mutation log (`Graph::drain_log` takes it whole). -/
  log : List α
  /-- `FlushLoop::pending`. -/
  pending : List α
  /-- Ghost: every batch that left `pending`, oldest first, with its fate. -/
  ledger : List (Fate × List α)
  /-- `FlushStats::dead_lettered`. -/
  deadLettered : Nat
  /-- `FlushLoop::retry_after` (ms on the tokio clock). -/
  retryAfter : Option Nat
  /-- `Shared::degraded`. -/
  degraded : Bool
  /-- The lease fence (`FlushTask::with_fence`). -/
  fenced : Bool
  /-- The loop has not exited. -/
  running : Bool
  /-- `store.flush` calls made so far: the next call's attempt number. -/
  attempts : Nat
  /-- Ghost: every `store.flush` call, in order, with its batch and outcome. -/
  calls : List (List α × Outcome)
  deriving DecidableEq, Repr

section Loop

variable {α : Type}

def St.init : St α :=
  { hist := [], log := [], pending := [], ledger := [], deadLettered := 0, retryAfter := none,
    degraded := false, fenced := false, running := true, attempts := 0, calls := [] }

/-- Every mutation that has left `pending`, in the order it left. -/
def St.consumed (s : St α) : List α := (s.ledger.map Prod.snd).flatten

/-- The post-retry hold (`RETAINED_BACKOFF`): `true` while a retained batch must wait. -/
def held (retryAfter : Option Nat) (now : Nat) : Bool :=
  match retryAfter with
  | some d => decide (now < d)
  | none => false

/-- What one ladder's result does to the loop state (the `match` in
`drain_and_flush`). `p` is the batch that was sent. -/
def settle (now : Nat) (erased : Bool) (p : List α) (r : Run α) (s : St α) : St α :=
  match r.result with
  | none => { s with pending := [], ledger := s.ledger ++ [(.committed, p)] }
  | some .constraint =>
    { s with pending := [], ledger := s.ledger ++ [(.deadLettered, p)],
             deadLettered := s.deadLettered + 1 }
  | some _ =>
    if erased then { s with fenced := true }
    else { s with retryAfter := some (now + r.sleeps.sum + retainedBackoff) }

/-- One `cycle` (`drain_and_flush`): drain, then flush when due. `tick` is an
interval tick (`false`: a `POLL_QUANTUM` poll); `erased` is what
`refused_as_erased` would answer after a failed flush (`Memory::build` always
arms the erased latch). -/
def cycle (P : Params) (st : Store α) (tick : Bool) (now : Nat) (erased : Bool) (s0 : St α) :
    St α :=
  let p := s0.pending ++ s0.log
  let s := { s0 with pending := p, log := [] }
  if p.isEmpty then s
  else if s.degraded then { s with pending := [], ledger := s.ledger ++ [(.droppedDegraded, p)] }
  else if P.logMax < p.length then { s with degraded := true }
  else if held s.retryAfter now then s
  else
    let s := { s with retryAfter := none }
    if !tick && decide (p.length < P.maxBatch) then s
    else
      let r := flushWithRetry st p s.attempts P.retries
      settle now erased p r
        { s with calls := s.calls ++ r.calls, attempts := s.attempts + r.calls.length }

/-- What the fence check does once the fence is latched: drop `pending`
(`drop_pending`), stop. -/
def dropFenced (s : St α) : St α :=
  { s with pending := [], running := false,
           ledger := if s.pending.isEmpty then s.ledger
                     else s.ledger ++ [(.droppedFenced, s.pending)] }

/-- `requeue_pending`: hand `pending` back to the FRONT of the graph log, stop. -/
def requeue (s : St α) : St α :=
  { s with log := s.pending ++ s.log, pending := [], running := false }

/-- Which `select!` branch fired. -/
inductive Wake where
  | stop
  | tick
  | poll
  deriving DecidableEq, Repr

/-- The fence check at the top of the loop (`if self.fenced()`). -/
def checkFence (s : St α) : St α := if s.fenced then dropFenced s else s

/-- One pass of the loop, from one `select!` to the next: the branch that
fired, then the top-of-loop fence check, which runs before the loop waits
again. A fence latched while the loop waits is therefore seen only after the
next branch has run; the stop branch exits without it. (The first check, at
spawn, finds `St.init` unfenced.) -/
def iter (P : Params) (st : Store α) (w : Wake) (now : Nat) (erased : Bool) (s : St α) : St α :=
  if !s.running then s
  else match w with
    | .stop => requeue s
    | .tick => checkFence (cycle P st true now erased s)
    | .poll => checkFence (cycle P st false now erased s)

/-- What can happen: a graph write, the heartbeat latching the fence, or a
loop wake-up. -/
inductive Ev (α : Type) where
  | write (m : α)
  | fence
  | wake (w : Wake) (now : Nat) (erased : Bool)
  deriving DecidableEq, Repr

def step (P : Params) (st : Store α) (s : St α) : Ev α → St α
  | .write m => { s with hist := s.hist ++ [m], log := s.log ++ [m] }
  | .fence => { s with fenced := true }
  | .wake w now erased => iter P st w now erased s

def run (P : Params) (st : Store α) (s : St α) (evs : List (Ev α)) : St α :=
  evs.foldl (step P st) s

/-- `JoinHandle::abort`: the task dies with `pending`. -/
def abort (s : St α) : St α := { s with pending := [], running := false }

/-! ### One cycle -/

section Cycle

variable {P : Params} {st : Store α} {tick : Bool} {now : Nat} {er : Bool} {s : St α}

theorem settle_shape (now : Nat) (er : Bool) (p : List α) (r : Run α) (s : St α) :
    (settle now er p r s).hist = s.hist ∧ (settle now er p r s).log = s.log ∧
    (settle now er p r s).running = s.running ∧ (settle now er p r s).calls = s.calls ∧
    (settle now er p r s).degraded = s.degraded ∧ s.fenced ≤ (settle now er p r s).fenced ∧
    (((settle now er p r s).pending = s.pending ∧ (settle now er p r s).ledger = s.ledger ∧
       (settle now er p r s).deadLettered = s.deadLettered) ∨
     ((settle now er p r s).pending = [] ∧
       (((settle now er p r s).ledger = s.ledger ++ [(.committed, p)] ∧
          (settle now er p r s).deadLettered = s.deadLettered) ∨
        ((settle now er p r s).ledger = s.ledger ++ [(.deadLettered, p)] ∧
          (settle now er p r s).deadLettered = s.deadLettered + 1)))) := by
  unfold settle
  split
  · simp
  · simp
  · split <;> simp

/-- What a cycle can do: keep the drained batch `p = pending ++ log` in
`pending`, or move it, whole, to the ledger with one fate. Every store call it
makes presents `p`. -/
theorem cycle_shape :
    (cycle P st tick now er s).hist = s.hist ∧ (cycle P st tick now er s).log = [] ∧
    (cycle P st tick now er s).running = s.running ∧
    s.degraded ≤ (cycle P st tick now er s).degraded ∧
    s.fenced ≤ (cycle P st tick now er s).fenced ∧
    (∃ new, (cycle P st tick now er s).calls = s.calls ++ new ∧
      ∀ c ∈ new, c.1 = s.pending ++ s.log) ∧
    (((cycle P st tick now er s).pending = s.pending ++ s.log ∧
       (cycle P st tick now er s).ledger = s.ledger ∧
       (cycle P st tick now er s).deadLettered = s.deadLettered) ∨
     ((cycle P st tick now er s).pending = [] ∧
       ((((cycle P st tick now er s).ledger = s.ledger ++ [(.committed, s.pending ++ s.log)] ∨
          ((cycle P st tick now er s).ledger = s.ledger ++ [(.droppedDegraded, s.pending ++ s.log)]
            ∧ (cycle P st tick now er s).degraded = true)) ∧
          (cycle P st tick now er s).deadLettered = s.deadLettered) ∨
        ((cycle P st tick now er s).ledger = s.ledger ++ [(.deadLettered, s.pending ++ s.log)] ∧
          (cycle P st tick now er s).deadLettered = s.deadLettered + 1)))) := by
  unfold cycle
  simp only []
  split
  · simp
  · split
    · rename_i _ hd
      simp [hd]
    · split
      · simp
      · split
        · simp
        · split
          · simp
          · have hs := settle_shape now er (s.pending ++ s.log)
              (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
              { s with pending := s.pending ++ s.log, log := [], retryAfter := none,
                       calls := s.calls ++
                         (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls,
                       attempts := s.attempts +
                         (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls.length }
            have hb := ladder_same_batch (st := st) (b := s.pending ++ s.log) s.attempts
              backoffBase P.retries
            simp only [] at hs
            obtain ⟨h1, h2, h3, h4, h5, h6, h7⟩ := hs
            refine ⟨h1, h2, h3, by rw [h5]; exact Bool.le_refl _, h6, ⟨_, h4, hb⟩, ?_⟩
            rcases h7 with h7 | ⟨h7a, h7b | h7b⟩
            · exact Or.inl h7
            · exact Or.inr ⟨h7a, Or.inl ⟨Or.inl h7b.1, h7b.2⟩⟩
            · exact Or.inr ⟨h7a, Or.inr h7b⟩

/-- The conditions under which a cycle reaches the store: it is not
degraded, has something pending within `log_max`, no post-retry hold is in
force, and it is an interval tick or the batch is full. (The fence is not
among them: the cycle never reads it, see `flush_after_fence_latched`.) -/
structure Due (P : Params) (tick : Bool) (now : Nat) (s : St α) : Prop where
  nonempty : (s.pending ++ s.log).isEmpty = false
  notDegraded : s.degraded = false
  withinMax : (s.pending ++ s.log).length ≤ P.logMax
  notHeld : held s.retryAfter now = false
  tickOrFull : tick = true ∨ P.maxBatch ≤ (s.pending ++ s.log).length

/-- A due cycle runs the ladder on `p = pending ++ log` and settles it. -/
theorem cycle_due (h : Due P tick now s) :
    cycle P st tick now er s =
      settle now er (s.pending ++ s.log) (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        { s with pending := s.pending ++ s.log, log := [], retryAfter := none,
                 calls := s.calls ++ (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls,
                 attempts := s.attempts +
                   (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls.length } := by
  have h1 := h.nonempty
  have h2 := h.notDegraded
  have h3 : ¬ P.logMax < (s.pending ++ s.log).length := Nat.not_lt.mpr h.withinMax
  have h4 := h.notHeld
  have h5 : (!tick && decide ((s.pending ++ s.log).length < P.maxBatch)) = false := by
    rcases h.tickOrFull with ht | hf
    · simp [ht]
    · cases tick
      · simp only [Bool.not_false, Bool.true_and, decide_eq_false_iff_not, Nat.not_lt]
        exact hf
      · simp
  unfold cycle
  simp only [h1, h2, h3, h4, h5]
  rfl

/-- **Post-retry hold.** While a retained batch's hold is in force, a wake-up makes
no store call. -/
theorem cycle_held (hheld : held s.retryAfter now = true) :
    (cycle P st tick now er s).calls = s.calls := by
  unfold cycle
  dsimp only
  by_cases h1 : (s.pending ++ s.log).isEmpty = true
  · rw [ite_eq_left h1]
  · rw [ite_eq_right h1]
    by_cases h2 : s.degraded = true
    · rw [ite_eq_left h2]
    · rw [ite_eq_right h2]
      by_cases h3 : P.logMax < (s.pending ++ s.log).length
      · rw [ite_eq_left h3]
      · rw [ite_eq_right h3, ite_eq_left hheld]

/-- **Early flush waits for a full batch.** A poll with fewer than
`max_batch` mutations pending makes no store call. -/
theorem cycle_poll_waits (hlt : (s.pending ++ s.log).length < P.maxBatch) :
    (cycle P st false now er s).calls = s.calls := by
  unfold cycle
  dsimp only
  by_cases h1 : (s.pending ++ s.log).isEmpty = true
  · rw [ite_eq_left h1]
  · rw [ite_eq_right h1]
    by_cases h2 : s.degraded = true
    · rw [ite_eq_left h2]
    · rw [ite_eq_right h2]
      by_cases h3 : P.logMax < (s.pending ++ s.log).length
      · rw [ite_eq_left h3]
      · rw [ite_eq_right h3]
        by_cases h4 : held s.retryAfter now = true
        · rw [ite_eq_left h4]
        · have h5 : (!false && decide ((s.pending ++ s.log).length < P.maxBatch)) = true := by
            simp only [Bool.not_false, Bool.true_and, decide_eq_true_eq]
            exact hlt
          rw [ite_eq_right h4, ite_eq_left h5]

end Cycle

/-! ### The loop's invariants -/

section Invariants

variable {P : Params} {st : Store α}

/-- What holds of every reachable state. -/
structure Good (s : St α) : Prop where
  /-- Conservation and order: the ledger, then `pending`, then the log, is
  the history. -/
  conserve : s.consumed ++ s.pending ++ s.log = s.hist
  /-- `dead_lettered` counts the dead-lettered batches. -/
  dead : s.deadLettered = (s.ledger.filter (fun e => e.1 == .deadLettered)).length
  /-- A batch dropped for degradation is only ever dropped once degraded. -/
  degr : ∀ e ∈ s.ledger, e.1 = .droppedDegraded → s.degraded = true
  /-- A batch dropped for the fence is only ever dropped once fenced. -/
  fence : ∀ e ∈ s.ledger, e.1 = .droppedFenced → s.fenced = true
  /-- A stopped loop holds nothing. -/
  stopped : s.running = false → s.pending = []

theorem good_init : Good (St.init : St α) where
  conserve := rfl
  dead := rfl
  degr := by simp [St.init]
  fence := by simp [St.init]
  stopped := by simp [St.init]

theorem good_cycle {tick : Bool} {now : Nat} {er : Bool} {s : St α} (hg : Good s)
    (hrun : s.running = true) : Good (cycle P st tick now er s) := by
  obtain ⟨h1, h2, h3, h4, h5, _, h7⟩ := cycle_shape (P := P) (st := st) (tick := tick)
    (now := now) (er := er) (s := s)
  have hle : ∀ {a b : Bool}, a ≤ b → a = true → b = true := by decide
  have hc := hg.conserve
  simp only [St.consumed] at hc
  have hrun' : (cycle P st tick now er s).running = true := by rw [h3, hrun]
  rcases h7 with ⟨hp, hl, hd⟩ | ⟨hp, ⟨hl, hd⟩ | ⟨hl, hd⟩⟩
  · constructor
    · simp only [St.consumed, hl, hp, h2, h1]
      simpa [List.append_assoc] using hc
    · rw [hd, hl]; exact hg.dead
    · intro e he hf; rw [hl] at he; exact hle h4 (hg.degr e he hf)
    · intro e he hf; rw [hl] at he; exact hle h5 (hg.fence e he hf)
    · intro hr; rw [hrun'] at hr; cases hr
  · constructor
    · simp only [St.consumed, hp, h2, h1]
      rcases hl with hl | ⟨hl, _⟩ <;>
      · rw [hl]
        simpa [List.append_assoc] using hc
    · rw [hd]
      rcases hl with hl | ⟨hl, _⟩ <;>
      · rw [hl, hg.dead]
        simp
    · intro e he hf
      rcases hl with hl | ⟨hl, hdeg⟩
      · rw [hl] at he
        simp only [List.mem_append, List.mem_singleton] at he
        rcases he with he | he
        · exact hle h4 (hg.degr e he hf)
        · rw [he] at hf; cases hf
      · exact hdeg
    · intro e he hf
      have he' : e ∈ s.ledger ∨ e.1 ≠ .droppedFenced := by
        rcases hl with hl | ⟨hl, _⟩ <;>
        · rw [hl] at he
          simp only [List.mem_append, List.mem_singleton] at he
          rcases he with he | he
          · exact Or.inl he
          · right; rw [he]; simp
      rcases he' with he' | he'
      · exact hle h5 (hg.fence e he' hf)
      · exact absurd hf he'
    · intro hr; rw [hrun'] at hr; cases hr
  · constructor
    · simp only [St.consumed, hp, h2, h1, hl]
      simpa [List.append_assoc] using hc
    · rw [hd, hl, hg.dead]
      simp
    · intro e he hf
      rw [hl] at he
      simp only [List.mem_append, List.mem_singleton] at he
      rcases he with he | he
      · exact hle h4 (hg.degr e he hf)
      · rw [he] at hf; cases hf
    · intro e he hf
      rw [hl] at he
      simp only [List.mem_append, List.mem_singleton] at he
      rcases he with he | he
      · exact hle h5 (hg.fence e he hf)
      · rw [he] at hf; cases hf
    · intro hr; rw [hrun'] at hr; cases hr

theorem good_dropFenced {s : St α} (hg : Good s) (hf : s.fenced = true) :
    Good (dropFenced s) := by
  by_cases he : s.pending.isEmpty = true
  · have hp : s.pending = [] := List.isEmpty_iff.mp he
    have e : dropFenced s = { s with pending := [], running := false } := by
      simp [dropFenced, he]
    rw [e]
    exact {
      conserve := by have := hg.conserve; rw [hp] at this; exact this
      dead := hg.dead
      degr := hg.degr
      fence := hg.fence
      stopped := fun _ => rfl }
  · have e : dropFenced s =
        { s with ledger := s.ledger ++ [(.droppedFenced, s.pending)], pending := [],
                 running := false } := by
      simp only [dropFenced, he]; rfl
    rw [e]
    exact {
      conserve := by
        have hc := hg.conserve
        simp only [St.consumed] at hc
        simp only [St.consumed, List.map_append, List.flatten_append]
        simpa [List.append_assoc] using hc
      dead := by
        simp only [List.filter_append]
        rw [hg.dead]
        simp
      degr := by
        intro e he hd
        simp only [List.mem_append, List.mem_singleton] at he ⊢
        rcases he with he | he
        · exact hg.degr e he hd
        · rw [he] at hd; cases hd
      fence := by intro _ _ _; exact hf
      stopped := by intro _; rfl }

theorem good_checkFence {s : St α} (hg : Good s) : Good (checkFence s) := by
  unfold checkFence
  by_cases hf : s.fenced = true
  · rw [ite_eq_left hf]; exact good_dropFenced hg hf
  · rw [ite_eq_right hf]; exact hg

theorem good_requeue {s : St α} (hg : Good s) : Good (requeue s) where
  conserve := by
    have hc := hg.conserve
    simp only [requeue, St.consumed] at hc ⊢
    simpa [List.append_assoc] using hc
  dead := hg.dead
  degr := hg.degr
  fence := hg.fence
  stopped := by intro _; rfl

theorem good_step {s : St α} (hg : Good s) (e : Ev α) : Good (step P st s e) := by
  cases e with
  | write m =>
    exact {
      conserve := by
        have hc := hg.conserve
        simp only [step, St.consumed] at hc ⊢
        rw [← hc]
        simp [List.append_assoc]
      dead := hg.dead
      degr := hg.degr
      fence := hg.fence
      stopped := hg.stopped }
  | fence =>
    exact {
      conserve := hg.conserve
      dead := hg.dead
      degr := hg.degr
      fence := by intro _ _ _; rfl
      stopped := hg.stopped }
  | wake w now er =>
    simp only [step, iter]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      cases w with
      | stop => exact good_requeue hg
      | tick => exact good_checkFence (good_cycle hg hr)
      | poll => exact good_checkFence (good_cycle hg hr)
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [hr']
      exact hg

theorem good_run {s : St α} (hg : Good s) (evs : List (Ev α)) : Good (run P st s evs) := by
  induction evs generalizing s with
  | nil => exact hg
  | cons e evs ih => exact ih (good_step hg e)

/-- Every reachable state is `Good`. -/
theorem good_reachable (evs : List (Ev α)) : Good (run P st St.init evs) :=
  good_run good_init evs

end Invariants

/-! ### Order: what the store is sent -/

section Order

variable {P : Params} {st : Store α}

theorem checkFence_shape (s : St α) :
    (checkFence s).hist = s.hist ∧ s.consumed <+: (checkFence s).consumed ∧
    (checkFence s).calls = s.calls := by
  unfold checkFence
  by_cases hf : s.fenced = true
  · rw [ite_eq_left hf]
    refine ⟨rfl, ⟨s.pending, ?_⟩, rfl⟩
    by_cases he : s.pending.isEmpty = true
    · simp [dropFenced, St.consumed, List.isEmpty_iff.mp he]
    · simp [dropFenced, St.consumed, he]
  · rw [ite_eq_right hf]
    exact ⟨rfl, List.prefix_refl _, rfl⟩

theorem step_shape (s : St α) (e : Ev α) :
    s.hist <+: (step P st s e).hist ∧ s.consumed <+: (step P st s e).consumed ∧
    ∃ new, (step P st s e).calls = s.calls ++ new ∧ ∀ c ∈ new, c.1 = s.pending ++ s.log := by
  cases e with
  | write m => exact ⟨⟨[m], rfl⟩, List.prefix_refl _, [], by simp [step], by simp⟩
  | fence => exact ⟨List.prefix_refl _, List.prefix_refl _, [], by simp [step], by simp⟩
  | wake w now er =>
    simp only [step, iter]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      have hcyc : ∀ tick, s.hist <+: (checkFence (cycle P st tick now er s)).hist ∧
          s.consumed <+: (checkFence (cycle P st tick now er s)).consumed ∧
          ∃ new, (checkFence (cycle P st tick now er s)).calls = s.calls ++ new ∧
            ∀ c ∈ new, c.1 = s.pending ++ s.log := by
        intro tick
        obtain ⟨h1, _, _, _, _, h6, h7⟩ := cycle_shape (P := P) (st := st) (tick := tick)
          (now := now) (er := er) (s := s)
        obtain ⟨e1, e2, e3⟩ := checkFence_shape (cycle P st tick now er s)
        rw [e1, e3]
        refine ⟨by rw [h1]; exact List.prefix_refl _, ?_, h6⟩
        refine List.IsPrefix.trans ?_ e2
        rcases h7 with ⟨_, hl, _⟩ | ⟨_, ⟨hl | ⟨hl, _⟩, _⟩ | ⟨hl, _⟩⟩
        · simp only [St.consumed, hl]; exact List.prefix_refl _
        all_goals
          exact ⟨s.pending ++ s.log, by simp [St.consumed, hl]⟩
      cases w with
      | stop => exact ⟨List.prefix_refl _, List.prefix_refl _, [], by simp [requeue], by simp⟩
      | tick => exact hcyc true
      | poll => exact hcyc false
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [hr']
      exact ⟨List.prefix_refl _, List.prefix_refl _, [], by simp, by simp⟩

/-- **Each batch sent is exactly the unconsumed tail of the history.** A store
call presents every mutation that is neither flushed, dead-lettered nor
dropped, oldest first: the retained batch leads, newly drained mutations
follow, nothing is skipped or reordered. -/
theorem step_calls_unconsumed {s : St α} (hg : Good s) (e : Ev α) :
    ∃ new, (step P st s e).calls = s.calls ++ new ∧
      ∀ c ∈ new, c.1 = s.hist.drop s.consumed.length := by
  obtain ⟨_, _, new, hnew, hb⟩ := step_shape (P := P) (st := st) s e
  refine ⟨new, hnew, fun c hc => ?_⟩
  rw [hb c hc, ← hg.conserve, List.append_assoc, List.drop_left]

/-- In particular a retained batch is always sent first: no later mutation
reaches the store ahead of it. -/
theorem retained_leads {s : St α} (e : Ev α) :
    ∃ new, (step P st s e).calls = s.calls ++ new ∧ ∀ c ∈ new, s.pending <+: c.1 := by
  obtain ⟨_, _, new, hnew, hb⟩ := step_shape (P := P) (st := st) s e
  exact ⟨new, hnew, fun c hc => by rw [hb c hc]; exact List.prefix_append _ _⟩

theorem prefix_drop {l₁ l₂ : List α} (h : l₁ <+: l₂) (k : Nat) : l₁.drop k <+: l₂.drop k := by
  obtain ⟨r, rfl⟩ := h
  rw [List.drop_append]
  by_cases hk : k ≤ l₁.length
  · have : k - l₁.length = 0 := by omega
    rw [this, List.drop_zero]
    exact List.prefix_append _ _
  · have : l₁.drop k = [] := List.drop_eq_nil_of_le (by omega)
    rw [this]
    simp

theorem run_cons (s : St α) (e : Ev α) (evs : List (Ev α)) :
    run P st s (e :: evs) = run P st (step P st s e) evs := rfl

theorem run_shape (s : St α) (evs : List (Ev α)) :
    s.hist <+: (run P st s evs).hist ∧ s.consumed <+: (run P st s evs).consumed ∧
    s.calls <+: (run P st s evs).calls := by
  induction evs generalizing s with
  | nil => exact ⟨List.prefix_refl _, List.prefix_refl _, List.prefix_refl _⟩
  | cons e evs ih =>
    obtain ⟨h1, h2, new, h3, _⟩ := step_shape (P := P) (st := st) s e
    obtain ⟨i1, i2, i3⟩ := ih (step P st s e)
    rw [run_cons]
    refine ⟨h1.trans i1, h2.trans i2, ?_⟩
    rw [h3] at i3
    exact (List.prefix_append _ _).trans i3

/-- **Nothing consumed is ever sent again.** Over any run, every store call
made after state `s` presents a segment of the history that starts at or
after the end of what `s` had already consumed. -/
theorem run_calls_after {s : St α} (hg : Good s) (evs : List (Ev α)) :
    ∀ c ∈ (run P st s evs).calls.drop s.calls.length,
      ∃ j, s.consumed.length ≤ j ∧ c.1 <+: (run P st s evs).hist.drop j := by
  induction evs generalizing s with
  | nil => simp [run]
  | cons e evs ih =>
    have h2 := (step_shape (P := P) (st := st) s e).2.1
    have hhist := (step_shape (P := P) (st := st) s e).1
    obtain ⟨new, h3, hb⟩ := step_calls_unconsumed (P := P) (st := st) hg e
    obtain ⟨i1, _, i3⟩ := run_shape (P := P) (st := st) (step P st s e) evs
    obtain ⟨r, hr⟩ := i3
    rw [run_cons]
    intro c hc
    have hc' := hc
    rw [← hr, h3, List.append_assoc, List.drop_left] at hc'
    rcases List.mem_append.mp hc' with hc' | hc'
    · refine ⟨s.consumed.length, Nat.le_refl _, ?_⟩
      rw [hb c hc']
      exact prefix_drop (hhist.trans i1) _
    · have := ih (good_step hg e) c (by rw [← hr, List.drop_left, h3] at *; exact hc')
      obtain ⟨j, hj, hpre⟩ := this
      exact ⟨j, Nat.le_trans h2.length_le hj, hpre⟩

/-- **A dead-lettered (or flushed, or dropped) mutation is never sent
again.** With mutations distinct (log entries are distinct events), no call
made after `s` contains anything `s` had already consumed. In particular a
dead-lettered batch is never retried. -/
theorem never_resent {s : St α} (hg : Good s) (evs : List (Ev α))
    (hnd : (run P st s evs).hist.Nodup) {m : α} (hm : m ∈ s.consumed) :
    ∀ c ∈ (run P st s evs).calls.drop s.calls.length, m ∉ c.1 := by
  intro c hc hmc
  obtain ⟨j, hj, hpre⟩ := run_calls_after (P := P) (st := st) hg evs c hc
  obtain ⟨hh, _, _⟩ := run_shape (P := P) (st := st) s evs
  have hcons : s.consumed <+: (run P st s evs).hist := by
    have : s.consumed <+: s.hist := by
      rw [← hg.conserve, List.append_assoc]; exact List.prefix_append _ _
    exact this.trans hh
  generalize (run P st s evs).hist = h at hcons hpre hnd
  obtain ⟨r, rfl⟩ := hcons
  have hm2 : m ∈ (s.consumed ++ r).drop j := hpre.subset hmc
  rw [List.drop_append, List.drop_eq_nil_of_le hj, List.nil_append] at hm2
  have hm3 : m ∈ r := List.mem_of_mem_drop hm2
  exact (List.nodup_append.mp hnd).2.2 m hm m hm3 rfl

/-- The committed mutations, in the order they were committed. -/
def St.committed (s : St α) : List α :=
  ((s.ledger.filter (fun e => e.1 == .committed)).map Prod.snd).flatten

theorem flatten_filter_sublist (q : Fate × List α → Bool) :
    ∀ l : List (Fate × List α),
      List.Sublist ((l.filter q).map Prod.snd).flatten (l.map Prod.snd).flatten
  | [] => List.Sublist.slnil
  | x :: l => by
    by_cases hq : q x = true
    · simp only [List.filter_cons, hq, ↓reduceIte, List.map_cons, List.flatten_cons]
      exact (flatten_filter_sublist q l).append_left _
    · simp only [List.filter_cons, hq, Bool.false_eq_true, ↓reduceIte, List.map_cons,
        List.flatten_cons]
      exact (flatten_filter_sublist q l).trans (List.sublist_append_right _ _)

/-- **The store is written in log order.** The committed mutations form a
subsequence of the history: what reaches the store keeps its log order. -/
theorem committed_sublist {s : St α} (hg : Good s) : List.Sublist s.committed s.hist := by
  have h1 : List.Sublist s.committed s.consumed := flatten_filter_sublist _ _
  have h2 : s.consumed <+: s.hist := by
    rw [← hg.conserve, List.append_assoc]; exact List.prefix_append _ _
  exact h1.trans h2.sublist

end Order

/-! ### The retry ladder inside the loop -/

section Outcomes

variable {P : Params} {st : Store α} {now : Nat} {er : Bool} {s : St α}

/-- Under `Due`, an interval tick of a running loop runs the ladder on
`p = pending ++ log`, settles it, and then runs the fence check. -/
theorem iter_due (hrun : s.running = true) (h : Due P true now s) :
    iter P st .tick now er s = checkFence
      (settle now er (s.pending ++ s.log) (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        { s with pending := s.pending ++ s.log, log := [], retryAfter := none,
                 calls := s.calls ++ (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls,
                 attempts := s.attempts +
                   (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls.length }) := by
  unfold iter
  rw [ite_eq_right (show ¬ ((!s.running) = true) by simp [hrun])]
  exact congrArg checkFence (cycle_due h)

theorem checkFence_unfenced {s : St α} (hf : s.fenced = false) : checkFence s = s := by
  simp [checkFence, hf]

/-- **Any due tick sends the retained batch first.** Whatever made it fail
before, a batch still in `pending` is sent again on the next due tick, ahead
of everything drained since. -/
theorem retained_resent (hrun : s.running = true) (h : Due P true now s) :
    ∃ rest, (iter P st .tick now er s).calls =
      s.calls ++ (s.pending ++ s.log, st (s.pending ++ s.log) s.attempts) :: rest := by
  rw [iter_due hrun h, (checkFence_shape _).2.2, (settle_shape _ _ _ _ _).2.2.2.1]
  have hpos := ladder_length_pos (st := st) (b := s.pending ++ s.log) s.attempts backoffBase
    P.retries
  have hsame := ladder_same_batch (st := st) (b := s.pending ++ s.log) s.attempts backoffBase
    P.retries
  have hidx := ladder_attempt_index (st := st) (b := s.pending ++ s.log) s.attempts backoffBase
    P.retries 0 hpos
  unfold flushWithRetry
  generalize (ladder st (s.pending ++ s.log) s.attempts backoffBase P.retries).calls = L
    at hpos hsame hidx
  cases L with
  | nil => simp at hpos
  | cons c rest =>
    refine ⟨rest, ?_⟩
    have hc1 : c.1 = s.pending ++ s.log := hsame c (List.mem_cons_self ..)
    have hc2 : c.2 = st (s.pending ++ s.log) s.attempts := by simpa using hidx
    have : c = (s.pending ++ s.log, st (s.pending ++ s.log) s.attempts) := Prod.ext hc1 hc2
    rw [this]

/-- **A retryable failure is retried, with the same batch.** When the first
attempt fails with a retryable error and `retries > 0`, the next store call
presents the same batch as attempt `n + 1`. -/
theorem transient_retried (hrun : s.running = true) (h : Due P true now s)
    {e : Err} (he : (st (s.pending ++ s.log) s.attempts).reported = some e)
    (hr : e.retryable = true) {k : Nat} (hk : P.retries = k + 1) :
    ∃ rest, (iter P st .tick now er s).calls =
      s.calls ++ (s.pending ++ s.log, st (s.pending ++ s.log) s.attempts) ::
        (s.pending ++ s.log, st (s.pending ++ s.log) (s.attempts + 1)) :: rest := by
  rw [iter_due hrun h, (checkFence_shape _).2.2, (settle_shape _ _ _ _ _).2.2.2.1]
  simp only [flushWithRetry, hk]
  rw [ladder_retry he hr]
  have hpos := ladder_length_pos (st := st) (b := s.pending ++ s.log) (s.attempts + 1)
    (nextBackoff backoffBase) k
  have hsame := ladder_same_batch (st := st) (b := s.pending ++ s.log) (s.attempts + 1)
    (nextBackoff backoffBase) k
  have hidx := ladder_attempt_index (st := st) (b := s.pending ++ s.log) (s.attempts + 1)
    (nextBackoff backoffBase) k 0 hpos
  generalize (ladder st (s.pending ++ s.log) (s.attempts + 1) (nextBackoff backoffBase) k).calls
    = L at hpos hsame hidx
  cases L with
  | nil => simp at hpos
  | cons c rest =>
    refine ⟨rest, ?_⟩
    have hc1 : c.1 = s.pending ++ s.log := hsame c (List.mem_cons_self ..)
    have hc2 : c.2 = st (s.pending ++ s.log) (s.attempts + 1) := by simpa using hidx
    have : c = (s.pending ++ s.log, st (s.pending ++ s.log) (s.attempts + 1)) :=
      Prod.ext hc1 hc2
    rw [this]

/-- **Exhausted retries retain the batch.** When the ladder ends in a
failure other than a constraint violation and the session is not erased,
nothing is dropped or counted: the whole batch stays in `pending` and the
post-retry hold is set. -/
theorem exhausted_retained (hrun : s.running = true) (hf : s.fenced = false)
    (h : Due P true now s) {e : Err}
    (hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some e)
    (hne : e ≠ .constraint) (her : er = false) :
    (iter P st .tick now er s).pending = s.pending ++ s.log ∧
    (iter P st .tick now er s).log = [] ∧
    (iter P st .tick now er s).ledger = s.ledger ∧
    (iter P st .tick now er s).deadLettered = s.deadLettered ∧
    (iter P st .tick now er s).running = true ∧
    (iter P st .tick now er s).retryAfter =
      some (now + (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).sleeps.sum
        + retainedBackoff) := by
  rw [iter_due hrun h]
  unfold checkFence settle
  rw [hres]
  cases e <;> simp_all

/-- **A constraint violation is dead-lettered exactly once.** One store
call, no retry and no backoff; the whole batch goes to the ledger as a dead
letter, `dead_lettered` goes up by one, and nothing is retained or held. -/
theorem constraint_dead_lettered (hrun : s.running = true) (hf : s.fenced = false)
    (h : Due P true now s)
    (hc : st (s.pending ++ s.log) s.attempts = .fail .constraint) :
    (iter P st .tick now er s).calls = s.calls ++ [(s.pending ++ s.log, .fail .constraint)] ∧
    (iter P st .tick now er s).ledger = s.ledger ++ [(.deadLettered, s.pending ++ s.log)] ∧
    (iter P st .tick now er s).deadLettered = s.deadLettered + 1 ∧
    (iter P st .tick now er s).pending = [] ∧
    (iter P st .tick now er s).retryAfter = none := by
  have hrep : (st (s.pending ++ s.log) s.attempts).reported = some .constraint := by
    rw [hc]; rfl
  have hl := ladder_nonretryable (st := st) (b := s.pending ++ s.log) (n := s.attempts)
    (bo := backoffBase) (k := P.retries) hrep rfl
  rw [iter_due hrun h]
  unfold checkFence settle flushWithRetry
  rw [hl, hc]
  simp [hf]

/-- **A stale write bails out of the ladder** (`is_retryable`: "bail out of
the retry ladder"): one store call, no backoff, whatever `retries` is. -/
theorem stale_write_one_call {b : List α} {n : Nat}
    (hs : st b n = .fail .staleWrite) (retries : Nat) :
    flushWithRetry st b n retries = ⟨[(b, .fail .staleWrite)], [], some .staleWrite⟩ := by
  have hrep : (st b n).reported = some .staleWrite := by rw [hs]; rfl
  unfold flushWithRetry
  rw [ladder_nonretryable hrep rfl, hs]

/-- **But a stale write is retained, not dead-lettered.** The doc on
`is_retryable` says the loop "treats it like a constraint"; the code's
`match` has a `Constraint` arm only, so a `StaleWrite` falls through to the
retain arm: the batch stays in `pending`, `dead_lettered` is unchanged, the
loop does not fence itself, and the post-retry hold is set exactly as for an outage. -/
theorem stale_write_retained (hrun : s.running = true) (hf : s.fenced = false)
    (h : Due P true now s) (hs : st (s.pending ++ s.log) s.attempts = .fail .staleWrite)
    (her : er = false) :
    (iter P st .tick now er s).calls = s.calls ++ [(s.pending ++ s.log, .fail .staleWrite)] ∧
    (iter P st .tick now er s).pending = s.pending ++ s.log ∧
    (iter P st .tick now er s).ledger = s.ledger ∧
    (iter P st .tick now er s).deadLettered = s.deadLettered ∧
    (iter P st .tick now er s).fenced = false ∧
    (iter P st .tick now er s).running = true ∧
    (iter P st .tick now er s).retryAfter = some (now + retainedBackoff) := by
  rw [iter_due hrun h, stale_write_one_call hs]
  simp [checkFence, settle, her, hf, hrun]

/-- An erased session's stale write latches the fence instead (#23), and
the fence check that follows drops the batch and stops the loop. -/
theorem stale_write_erased (hrun : s.running = true) (h : Due P true now s)
    (hs : st (s.pending ++ s.log) s.attempts = .fail .staleWrite) :
    (iter P st .tick now true s).calls = s.calls ++ [(s.pending ++ s.log, .fail .staleWrite)] ∧
    (iter P st .tick now true s).ledger = s.ledger ++ [(.droppedFenced, s.pending ++ s.log)] ∧
    (iter P st .tick now true s).deadLettered = s.deadLettered ∧
    (iter P st .tick now true s).running = false := by
  rw [iter_due hrun h, stale_write_one_call hs]
  have hne := h.nonempty
  simp [checkFence, settle, dropFenced]
  simpa using hne

end Outcomes

/-! ### Fence, degradation, stop -/

section Stop

variable {P : Params} {st : Store α}

/-- A stopped loop changes nothing but the graph log. -/
theorem stopped_silent {s : St α} (hr : s.running = false) (e : Ev α) :
    (step P st s e).calls = s.calls ∧ (step P st s e).running = false ∧
    (step P st s e).ledger = s.ledger ∧ (step P st s e).pending = s.pending := by
  cases e with
  | write m => exact ⟨rfl, hr, rfl, rfl⟩
  | fence => exact ⟨rfl, hr, rfl, rfl⟩
  | wake w now er => simp [step, iter, hr]

theorem run_stopped_silent {s : St α} (hr : s.running = false) (evs : List (Ev α)) :
    (run P st s evs).calls = s.calls ∧ (run P st s evs).ledger = s.ledger := by
  induction evs generalizing s with
  | nil => exact ⟨rfl, rfl⟩
  | cons e evs ih =>
    rw [run_cons]
    obtain ⟨h1, h2, h3, _⟩ := stopped_silent (P := P) (st := st) hr e
    rw [(ih h2).1, (ih h2).2, h1, h3]
    exact ⟨rfl, rfl⟩

/-- **A latched fence stops the loop at its next wake-up**, whatever the
branch: after one more pass nothing can reach the store again
(`run_stopped_silent`). -/
theorem fenced_stops {s : St α} (hf : s.fenced = true) (w : Wake) (now : Nat) (er : Bool) :
    (step P st s (.wake w now er)).running = false := by
  simp only [step, iter]
  by_cases hr : s.running = true
  · have hr' : (!s.running) = false := by simp [hr]
    rw [hr']
    simp only [Bool.false_eq_true, ↓reduceIte]
    cases w with
    | stop => rfl
    | tick | poll =>
      have hcf : ∀ tick, (cycle P st tick now er s).fenced = true := by
        intro tick
        have := (cycle_shape (P := P) (st := st) (tick := tick) (now := now) (er := er)
          (s := s)).2.2.2.2.1
        rw [hf] at this
        revert this; cases (cycle P st tick now er s).fenced <;> decide
      simp [checkFence, hcf, dropFenced]
  · simp at hr; simp [hr]

/-- **The fence is seen one pass late.** The check sits at the top of the
loop, before the `select!` wait, so a fence latched during the wait is not
seen by the branch that ends the wait: a due tick still sends the batch to the
store, and only the check after it drops the tail. The wait is at most one
`POLL_QUANTUM` (100 ms), so in the Rust this is a further flush only when the
fence latches within 100 ms before a due tick, or while `pending` holds at
least `max_batch` mutations. The flush.rs comment says "once the heartbeat
latches a lost lease no further flush is issued"; this is one further flush
(at most one ladder), which the store-side token refuses
(`flush_after_takeover`). -/
theorem flush_after_fence_latched {s : St α} {now : Nat} {er : Bool} (hrun : s.running = true)
    (hf : s.fenced = true) (h : Due P true now s) :
    (∃ rest, (iter P st .tick now er s).calls =
      s.calls ++ (s.pending ++ s.log, st (s.pending ++ s.log) s.attempts) :: rest) ∧
    (iter P st .tick now er s).running = false :=
  ⟨retained_resent hrun h, fenced_stops (P := P) (st := st) hf .tick now er⟩

/-- **And a stop that ends the wait hands the tail back, fenced or not.** The
stop branch exits without the fence check, so a fence latched during the wait
does not drop `pending`: it goes back to the graph log like any other stop
(the flush.rs comment says a fenced tail is dropped, never handed back for
`close`'s final flush; `Memory::close` checks `lease_lost` itself before it
stops the loop). The equation is definitional (`iter`'s stop branch); the
fenced case is the `decide` example after `exStale`. -/
theorem stop_requeues {s : St α} (hr : s.running = true) (now : Nat) (er : Bool) :
    step P st s (.wake .stop now er) = requeue s ∧
    (requeue s).log = s.pending ++ s.log ∧ (requeue s).pending = [] ∧
    (requeue s).ledger = s.ledger ∧ (requeue s).calls = s.calls ∧
    (requeue s).running = false := by
  refine ⟨?_, rfl, rfl, rfl, rfl, rfl⟩
  simp [step, iter, hr]

/-- **Once degraded, no store call is ever made again** (`durability="none"`,
terminal). -/
theorem degraded_silent {s : St α} (hd : s.degraded = true) (e : Ev α) :
    (step P st s e).calls = s.calls ∧ (step P st s e).degraded = true := by
  cases e with
  | write m => exact ⟨rfl, hd⟩
  | fence => exact ⟨rfl, hd⟩
  | wake w now er =>
    simp only [step, iter]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      have hcyc : ∀ tick now er, (checkFence (cycle P st tick now er s)).calls = s.calls ∧
          (checkFence (cycle P st tick now er s)).degraded = true := by
        intro tick now er
        have hc : (cycle P st tick now er s).calls = s.calls ∧
            (cycle P st tick now er s).degraded = true := by
          unfold cycle
          dsimp only
          by_cases h1 : (s.pending ++ s.log).isEmpty = true
          · rw [ite_eq_left h1]; exact ⟨rfl, hd⟩
          · rw [ite_eq_right h1, ite_eq_left hd]; exact ⟨rfl, hd⟩
        rw [(checkFence_shape _).2.2]
        refine ⟨hc.1, ?_⟩
        unfold checkFence
        split
        · exact hc.2
        · exact hc.2
      cases w with
      | stop => exact ⟨rfl, hd⟩
      | tick => exact hcyc _ _ _
      | poll => exact hcyc _ _ _
    · simp at hr; simp [hr, hd]

theorem run_degraded_silent {s : St α} (hd : s.degraded = true) (evs : List (Ev α)) :
    (run P st s evs).calls = s.calls := by
  induction evs generalizing s with
  | nil => rfl
  | cons e evs ih =>
    rw [run_cons, ih (degraded_silent hd e).2, (degraded_silent hd e).1]

/-- **After shutdown nothing is unaccounted for.** In every reachable state
whose loop has exited, the history is exactly the ledger followed by the
graph log: every mutation was committed, dead-lettered, dropped by a
degraded or fenced session, or is back in the graph log for
`Memory::close`'s final flush. -/
theorem stopped_accounted {s : St α} (hg : Good s) (hr : s.running = false) :
    s.consumed ++ s.log = s.hist := by
  have := hg.conserve
  rw [hg.stopped hr, List.append_nil] at this
  exact this

/-- **No silent loss.** Every batch that left `pending` without reaching the
store left it observably: a dead letter is counted in `dead_lettered`, a
degraded drop happens only once `degraded()` is set, and a fenced drop only
once the fence is latched. -/
theorem no_silent_loss {s : St α} (hg : Good s) :
    ∀ e ∈ s.ledger, e.1 = .committed ∨ (e.1 = .deadLettered ∧ 0 < s.deadLettered) ∨
      (e.1 = .droppedDegraded ∧ s.degraded = true) ∨ (e.1 = .droppedFenced ∧ s.fenced = true) := by
  intro e he
  cases hf : e.1 with
  | committed => exact Or.inl rfl
  | deadLettered =>
    refine Or.inr (Or.inl ⟨rfl, ?_⟩)
    rw [hg.dead]
    exact List.length_pos_of_mem (List.mem_filter.mpr ⟨he, by rw [hf]; rfl⟩)
  | droppedDegraded => exact Or.inr (Or.inr (Or.inl ⟨rfl, hg.degr e he hf⟩))
  | droppedFenced => exact Or.inr (Or.inr (Or.inr ⟨rfl, hg.fence e he hf⟩))

/-- **The one documented loss: a hard abort.** `JoinHandle::abort` drops a
non-empty `pending` with the task: afterwards the history is no longer
accounted for ("A hard `abort` still works but DROPS `pending`"). -/
theorem abort_loses {s : St α} (hg : Good s) (hp : s.pending ≠ []) :
    (abort s).consumed ++ (abort s).pending ++ (abort s).log ≠ (abort s).hist := by
  intro h
  have hc := hg.conserve
  have e1 : (abort s).consumed = s.consumed := rfl
  have e2 : (abort s).pending = [] := rfl
  have e3 : (abort s).log = s.log := rfl
  have e4 : (abort s).hist = s.hist := rfl
  rw [e1, e2, e3, e4, ← hc] at h
  have hl := congrArg List.length h
  simp only [List.length_append, List.length_nil] at hl
  have : 0 < s.pending.length := List.length_pos_iff.mpr hp
  omega

end Stop

end Loop

/-! ## The fencing token makes a stale retry harmless -/

section Fencing

variable {α : Type} {H : Type} [DecidableEq H]

/-- `lease_permits_write` as a `Bool` (`Lease.Permits`). -/
def permits (current : Nat) : Option Nat → Bool
  | none => current == 0
  | some p => current == 0 || decide (current ≤ p)

theorem permits_iff (current : Nat) (t : Option Nat) :
    permits current t = true ↔ Lease.Permits current t := by
  unfold Lease.Permits
  cases t with
  | none => simp [permits]
  | some p =>
    simp only [permits, Bool.or_eq_true, beq_iff_eq, decide_eq_true_eq]
    constructor
    · intro h
      rcases h with h | h
      · exact Or.inl h
      · exact Or.inr ⟨p, rfl, h⟩
    · intro h
      rcases h with h | ⟨q, hq, hle⟩
      · exact Or.inl h
      · exact Or.inr (by cases hq; exact hle)

/-- A store that runs the fence in front of `inner`: attempt `n` reads lease
row `row n` in its own transaction and refuses a stale token with
`StaleWrite` before writing anything. -/
def fencing (row : Nat → Lease.Row H) (token : Option Nat) (inner : Store α) : Store α :=
  fun b n => if permits (row n).token token then inner b n else .fail .staleWrite

/-- **A retry after a takeover never lands.** Once another live identity
holds the lease (from attempt `n0` on), every attempt the loop makes with its
fixed token is refused, so re-sending a retained batch after a `StaleWrite`
writes nothing (`Lease.stale_token_refused`). -/
theorem fencing_refuses_after_takeover {released A : H} {a : Lease.Row H} (hA : a.holder = A)
    (row : Nat → Lease.Row H) (inner : Store α) (n0 : Nat)
    (hsteps : ∀ n, n0 ≤ n → Lease.Steps released a (row n))
    (hother : ∀ n, n0 ≤ n → (row n).holder ≠ A ∧ (row n).holder ≠ released) :
    ∀ b n, n0 ≤ n → fencing row (some a.token) inner b n = .fail .staleWrite := by
  intro b n hn
  have hno := Lease.stale_token_refused hA (hsteps n hn) (hother n hn).1 (hother n hn).2
  have : permits (row n).token (some a.token) = false := by
    cases hp : permits (row n).token (some a.token) with
    | false => rfl
    | true => exact absurd ((permits_iff _ _).mp hp) hno
  simp [fencing, this]

/-- So a whole flush attempt after the takeover is one refused call that
commits nothing. -/
theorem flush_after_takeover {released A : H} {a : Lease.Row H} (hA : a.holder = A)
    (row : Nat → Lease.Row H) (inner : Store α) (n0 : Nat)
    (hsteps : ∀ n, n0 ≤ n → Lease.Steps released a (row n))
    (hother : ∀ n, n0 ≤ n → (row n).holder ≠ A ∧ (row n).holder ≠ released)
    (b : List α) {n : Nat} (hn : n0 ≤ n) (retries : Nat) :
    flushWithRetry (fencing row (some a.token) inner) b n retries =
      ⟨[(b, .fail .staleWrite)], [], some .staleWrite⟩ :=
  stale_write_one_call (fencing_refuses_after_takeover hA row inner n0 hsteps hother b n hn) retries

/-- **A refresh can re-take the lease, and the flush loop does not follow.**
The heartbeat's refresh is an acquire (`GraphStore::refresh_lease`). After the
documented `OPERATOR_OVERRIDE` (here `Lease.release`: holder
`lambo:released`, row expired, token kept), the wedged holder `A`'s own next
refresh takes the row over: `A` holds it again, so the heartbeat gets
`Acquired` and never latches the fence, but at `token + 1`, which the
heartbeat ignores (`Ok(LeaseOutcome::Acquired(_)) => {}`). The flush loop still
presents the token it was built with, so every attempt is refused as
`StaleWrite`, for as long as this row stands. With `stale_write_retained`
the batch is retained and resent every `RETAINED_BACKOFF`, indefinitely. -/
theorem override_refresh_retakes {released A : H} (a : Lease.Row H) (hA : a.holder = A)
    (hAr : A ≠ released) (now now' ttl : Int) (hle : now ≤ now') (inner : Store α) :
    (Lease.acquire A now' ttl (Lease.release released A now a)).holder = A ∧
    (Lease.acquire A now' ttl (Lease.release released A now a)).token = a.token + 1 ∧
    ∀ b n, fencing (fun _ => Lease.acquire A now' ttl (Lease.release released A now a))
      (some a.token) inner b n = .fail .staleWrite := by
  have htok := Lease.acquire_after_release_bumps released A A now now' ttl a hA hAr hle
  have hhold : (Lease.acquire A now' ttl (Lease.release released A now a)).holder = A := by
    rw [Lease.release_mine hA]
    have h2 : ({ holder := released, token := a.token, expires := now } : Lease.Row H).holder
        ≠ A := fun e => hAr e.symm
    have h1 : ¬ (now' < ({ holder := released, token := a.token, expires := now } :
        Lease.Row H).expires ∧ ({ holder := released, token := a.token, expires := now } :
        Lease.Row H).holder ≠ A) := by
      intro hc
      have : now' < now := hc.1
      omega
    rw [Lease.acquire_takeover h1 h2]
  refine ⟨hhold, htok, ?_⟩
  intro b n
  have hp : permits (Lease.acquire A now' ttl (Lease.release released A now a)).token
      (some a.token) = false := by
    rw [htok]
    simp [permits]
  simp [fencing, hp]

end Fencing

/-! ## The fix for #293

The definitions above stay the model of the loop before #293. The fixed loop
differs in three places, each a `Fixed` definition beside its original:

* `settleFixed`: a `StaleWrite` is terminal. Like the erased arm it latches the
  fence, so the fence check that follows drops `pending` and stops the loop;
  it is never retained or sent again.
* `cycleFixed`: the fence is re-checked right after the drain, before anything
  can reach the store, so a fence latched while the loop waited is honoured by
  the branch that ends the wait.
* `iterFixed`: the stop branch checks the fence too, and drops a fenced tail
  instead of handing it back to the graph log. -/

section Fixed

variable {α : Type}

/-- `drain_and_flush`'s `match` after #293. A `StaleWrite` (erased or not)
latches the fence; everything else is `settle`. -/
def settleFixed (now : Nat) (erased : Bool) (p : List α) (r : Run α) (s : St α) : St α :=
  match r.result with
  | none => { s with pending := [], ledger := s.ledger ++ [(.committed, p)] }
  | some .constraint =>
    { s with pending := [], ledger := s.ledger ++ [(.deadLettered, p)],
             deadLettered := s.deadLettered + 1 }
  | some .staleWrite => { s with fenced := true }
  | some _ =>
    if erased then { s with fenced := true }
    else { s with retryAfter := some (now + r.sleeps.sum + retainedBackoff) }

/-- `drain_and_flush` after #293: the fence is re-checked after the drain,
before the early returns and the ladder, and the ladder's result is settled by
`settleFixed`. -/
def cycleFixed (P : Params) (st : Store α) (tick : Bool) (now : Nat) (erased : Bool) (s0 : St α) :
    St α :=
  let p := s0.pending ++ s0.log
  let s := { s0 with pending := p, log := [] }
  if s.fenced then s
  else if p.isEmpty then s
  else if s.degraded then { s with pending := [], ledger := s.ledger ++ [(.droppedDegraded, p)] }
  else if P.logMax < p.length then { s with degraded := true }
  else if held s.retryAfter now then s
  else
    let s := { s with retryAfter := none }
    if !tick && decide (p.length < P.maxBatch) then s
    else
      let r := flushWithRetry st p s.attempts P.retries
      settleFixed now erased p r
        { s with calls := s.calls ++ r.calls, attempts := s.attempts + r.calls.length }

/-- One pass of the fixed loop. The stop branch drops a fenced tail; the
cycle branches are followed by the top-of-loop fence check as before. -/
def iterFixed (P : Params) (st : Store α) (w : Wake) (now : Nat) (erased : Bool) (s : St α) :
    St α :=
  if !s.running then s
  else match w with
    | .stop => if s.fenced then dropFenced s else requeue s
    | .tick => checkFence (cycleFixed P st true now erased s)
    | .poll => checkFence (cycleFixed P st false now erased s)

def stepFixed (P : Params) (st : Store α) (s : St α) : Ev α → St α
  | .write m => { s with hist := s.hist ++ [m], log := s.log ++ [m] }
  | .fence => { s with fenced := true }
  | .wake w now erased => iterFixed P st w now erased s

def runFixed (P : Params) (st : Store α) (s : St α) (evs : List (Ev α)) : St α :=
  evs.foldl (stepFixed P st) s

theorem runFixed_cons {P : Params} {st : Store α} (s : St α) (e : Ev α) (evs : List (Ev α)) :
    runFixed P st s (e :: evs) = runFixed P st (stepFixed P st s e) evs := rfl

/-! ### The fixed loop is the old loop with two substitutions -/

/-- The drain alone. -/
def drained (s : St α) : St α := { s with pending := s.pending ++ s.log, log := [] }

/-- The erased flag the old `settle` would need to behave like `settleFixed`
on this state's ladder: set when the ladder ends in `StaleWrite`. -/
def erasedFixed (P : Params) (st : Store α) (er : Bool) (s : St α) : Bool :=
  er || decide ((flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result =
    some .staleWrite)

theorem settleFixed_eq (now : Nat) (er : Bool) (p : List α) (r : Run α) (s : St α) :
    settleFixed now er p r s = settle now (er || decide (r.result = some .staleWrite)) p r s := by
  unfold settleFixed settle
  cases h : r.result with
  | none => simp
  | some e => cases e <;> simp

/-- A fenced cycle only drains; an unfenced one is the old cycle, with a
`StaleWrite` treated as the old code treats an erased session's. -/
theorem cycleFixed_eq (P : Params) (st : Store α) (tick : Bool) (now : Nat) (er : Bool)
    (s : St α) :
    cycleFixed P st tick now er s =
      if s.fenced then drained s else cycle P st tick now (erasedFixed P st er s) s := by
  by_cases hf : s.fenced = true
  · simp [cycleFixed, drained, hf]
  · have hf' : s.fenced = false := by simpa using hf
    simp only [hf', Bool.false_eq_true, ↓reduceIte]
    unfold cycleFixed cycle erasedFixed
    simp only [hf', Bool.false_eq_true, ↓reduceIte, settleFixed_eq]

theorem cycleFixed_fenced {P : Params} {st : Store α} {tick : Bool} {now : Nat} {er : Bool}
    {s : St α} (hf : s.fenced = true) : cycleFixed P st tick now er s = drained s := by
  rw [cycleFixed_eq]; simp [hf]

theorem cycleFixed_unfenced {P : Params} {st : Store α} {tick : Bool} {now : Nat} {er : Bool}
    {s : St α} (hf : s.fenced = false) :
    cycleFixed P st tick now er s = cycle P st tick now (erasedFixed P st er s) s := by
  rw [cycleFixed_eq]; simp [hf]

/-- **Agreement.** Where the fix should change nothing, it changes nothing: in
a state whose fence is not latched and whose next ladder would not end in
`StaleWrite`, every event does exactly what it did before. -/
theorem stepFixed_agrees {P : Params} {st : Store α} {s : St α} (hf : s.fenced = false)
    (hs : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result ≠
      some .staleWrite) (e : Ev α) :
    stepFixed P st s e = step P st s e := by
  have her : ∀ er, erasedFixed P st er s = er := by
    intro er
    simp [erasedFixed, hs]
  cases e with
  | write m => rfl
  | fence => rfl
  | wake w now er =>
    simp only [stepFixed, step, iterFixed, iter]
    cases w with
    | stop => simp [hf]
    | tick => simp only [cycleFixed_unfenced hf, her]
    | poll => simp only [cycleFixed_unfenced hf, her]

/-! ### The invariants still hold -/

section FixedInvariants

variable {P : Params} {st : Store α}

theorem good_drained {s : St α} (hg : Good s) (hrun : s.running = true) : Good (drained s) where
  conserve := by
    have hc := hg.conserve
    simp only [drained, St.consumed] at hc ⊢
    simpa [List.append_assoc] using hc
  dead := hg.dead
  degr := hg.degr
  fence := hg.fence
  stopped := by intro h; simp [drained, hrun] at h

theorem good_cycleFixed {tick : Bool} {now : Nat} {er : Bool} {s : St α} (hg : Good s)
    (hrun : s.running = true) : Good (cycleFixed P st tick now er s) := by
  rw [cycleFixed_eq]
  split
  · exact good_drained hg hrun
  · exact good_cycle hg hrun

theorem good_stepFixed {s : St α} (hg : Good s) (e : Ev α) : Good (stepFixed P st s e) := by
  cases e with
  | write m => exact good_step (P := P) (st := st) hg (.write m)
  | fence => exact good_step (P := P) (st := st) hg .fence
  | wake w now er =>
    simp only [stepFixed, iterFixed]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      cases w with
      | stop =>
        by_cases hf : s.fenced = true
        · simp only [hf, ↓reduceIte]; exact good_dropFenced hg hf
        · simp only [hf, Bool.false_eq_true, ↓reduceIte]; exact good_requeue hg
      | tick => exact good_checkFence (good_cycleFixed hg hr)
      | poll => exact good_checkFence (good_cycleFixed hg hr)
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [hr']
      exact hg

theorem good_runFixed {s : St α} (hg : Good s) (evs : List (Ev α)) :
    Good (runFixed P st s evs) := by
  induction evs generalizing s with
  | nil => exact hg
  | cons e evs ih => exact ih (good_stepFixed hg e)

/-- **`good_reachable` for the fixed loop.** Conservation, the dead-letter
count, and "dropped only once degraded or fenced" hold of every state the
fixed loop reaches. -/
theorem good_reachable_fixed (evs : List (Ev α)) : Good (runFixed P st St.init evs) :=
  good_runFixed good_init evs

/-- **`no_silent_loss` for the fixed loop.** Of the loop alone: a batch
dropped for a stale write is a fenced drop, made only once the fence is
latched, the flag `Memory` refuses writes on. That `close` reports it is a
separate obligation, met only by a close that reads the fence after the loop
has stopped (`close_ok_no_drop`; `close_ok_over_drop` is the close that read
it only before). -/
theorem no_silent_loss_fixed (evs : List (Ev α)) :
    ∀ e ∈ (runFixed P st St.init evs).ledger, e.1 = .committed ∨
      (e.1 = .deadLettered ∧ 0 < (runFixed P st St.init evs).deadLettered) ∨
      (e.1 = .droppedDegraded ∧ (runFixed P st St.init evs).degraded = true) ∨
      (e.1 = .droppedFenced ∧ (runFixed P st St.init evs).fenced = true) :=
  no_silent_loss (good_reachable_fixed evs)

/-- `stopped_accounted` for the fixed loop. -/
theorem stopped_accounted_fixed (evs : List (Ev α))
    (hr : (runFixed P st St.init evs).running = false) :
    (runFixed P st St.init evs).consumed ++ (runFixed P st St.init evs).log =
      (runFixed P st St.init evs).hist :=
  stopped_accounted (good_reachable_fixed evs) hr

end FixedInvariants

/-! ### No flush after the fence, no resend after a stale write -/

section FixedFence

variable {P : Params} {st : Store α}

/-- Once the fence is latched, no event makes a store call, and the fence
stays latched. -/
theorem stepFixed_fenced {s : St α} (hf : s.fenced = true) (e : Ev α) :
    (stepFixed P st s e).calls = s.calls ∧ (stepFixed P st s e).fenced = true := by
  cases e with
  | write m => exact ⟨rfl, hf⟩
  | fence => exact ⟨rfl, rfl⟩
  | wake w now er =>
    simp only [stepFixed, iterFixed]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      cases w with
      | stop => simp [hf, dropFenced]
      | tick => simp [cycleFixed_fenced hf, hf, drained, checkFence, dropFenced]
      | poll => simp [cycleFixed_fenced hf, hf, drained, checkFence, dropFenced]
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [hr']
      exact ⟨rfl, hf⟩

/-- **No flush is issued after the fence latches.** From a fenced state, no
run of the fixed loop ever calls the store again. -/
theorem runFixed_fenced_silent {s : St α} (hf : s.fenced = true) (evs : List (Ev α)) :
    (runFixed P st s evs).calls = s.calls := by
  induction evs generalizing s with
  | nil => rfl
  | cons e evs ih =>
    rw [runFixed_cons, ih (stepFixed_fenced hf e).2, (stepFixed_fenced hf e).1]

/-- In particular, whatever the loop is doing when the heartbeat latches the
fence, nothing reaches the store afterwards. -/
theorem no_flush_after_fence_fixed (s : St α) (evs : List (Ev α)) :
    (runFixed P st s (.fence :: evs)).calls = s.calls := by
  rw [runFixed_cons]
  exact runFixed_fenced_silent (s := stepFixed P st s .fence) rfl evs

/-- **The fence is seen by the branch that ends the wait.** The scenario of
`flush_after_fence_latched` (the fence latched while the loop waited, then a
due tick): the fixed loop makes no store call, drops the drained tail as a
fenced drop and stops. -/
theorem fence_before_due_tick_fixed {s : St α} {now : Nat} {er : Bool}
    (hrun : s.running = true) (hf : s.fenced = true) (h : Due P true now s) :
    (iterFixed P st .tick now er s).calls = s.calls ∧
    (iterFixed P st .tick now er s).ledger = s.ledger ++ [(.droppedFenced, s.pending ++ s.log)] ∧
    (iterFixed P st .tick now er s).log = [] ∧
    (iterFixed P st .tick now er s).running = false := by
  have hne : (s.pending ++ s.log).isEmpty = false := h.nonempty
  have hr' : (!s.running) = false := by simp [hrun]
  simp only [iterFixed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleFixed_eq]
  simp only [hf, ↓reduceIte]
  have hne' : (s.pending ++ s.log).isEmpty = false := hne
  simp [drained, checkFence, dropFenced, hf, hne']

/-- **A stop after the fence drops the tail.** The fixed stop branch checks
the fence: a fenced `pending` goes to the ledger as a fenced drop and is not
handed back to the graph log for `close`'s final flush (contrast
`stop_requeues`). -/
theorem stop_drops_fenced {s : St α} (hr : s.running = true) (hf : s.fenced = true)
    (now : Nat) (er : Bool) :
    stepFixed P st s (.wake .stop now er) = dropFenced s ∧
    (dropFenced s).log = s.log ∧ (dropFenced s).calls = s.calls ∧
    (dropFenced s).running = false := by
  refine ⟨?_, rfl, rfl, rfl⟩
  simp [stepFixed, iterFixed, hr, hf]

/-- A call to the store that reports a non-retryable error is the ladder's last,
and the ladder's result. -/
theorem ladder_terminal_result {b : List α} (n bo k : Nat) :
    ∀ c ∈ (ladder st b n bo k).calls, ∀ e, c.2.reported = some e → e.retryable = false →
      (ladder st b n bo k).result = some e := by
  induction k generalizing n bo with
  | zero =>
    intro c hc e he _
    simp only [ladder, List.mem_singleton] at hc ⊢
    rw [← he, hc]
  | succ k ih =>
    intro c hc e he hr
    cases h : (st b n).reported with
    | none =>
      rw [ladder_ok h] at hc ⊢
      simp only [List.mem_singleton] at hc
      rw [hc] at he
      rw [h] at he
      cases he
    | some e0 =>
      cases hr0 : e0.retryable with
      | false =>
        rw [ladder_nonretryable h hr0] at hc ⊢
        simp only [List.mem_singleton] at hc
        rw [hc, h] at he
        exact congrArg some (Option.some.inj he)
      | true =>
        rw [ladder_retry h hr0] at hc ⊢
        cases hc with
        | head =>
          rw [h] at he
          cases Option.some.inj he
          rw [hr0] at hr
          cases hr
        | tail _ hc => exact ih _ _ c hc e he hr

/-- What a cycle does, as far as the store is concerned: it makes no call, or
it is due. -/
theorem cycle_calls_or_due {tick : Bool} {now : Nat} {er : Bool} (s : St α) :
    (cycle P st tick now er s).calls = s.calls ∨ Due P tick now s := by
  unfold cycle
  dsimp only
  by_cases h1 : (s.pending ++ s.log).isEmpty = true
  · rw [ite_eq_left h1]; exact Or.inl rfl
  · rw [ite_eq_right h1]
    by_cases h2 : s.degraded = true
    · rw [ite_eq_left h2]; exact Or.inl rfl
    · rw [ite_eq_right h2]
      by_cases h3 : P.logMax < (s.pending ++ s.log).length
      · rw [ite_eq_left h3]; exact Or.inl rfl
      · rw [ite_eq_right h3]
        by_cases h4 : held s.retryAfter now = true
        · rw [ite_eq_left h4]; exact Or.inl rfl
        · rw [ite_eq_right h4]
          by_cases h5 : (!tick && decide ((s.pending ++ s.log).length < P.maxBatch)) = true
          · rw [ite_eq_left h5]; exact Or.inl rfl
          · right
            refine ⟨by simpa using h1, by simpa using h2, by omega, by simpa using h4, ?_⟩
            cases tick
            · right
              simp only [Bool.not_false, Bool.true_and, decide_eq_true_eq] at h5
              omega
            · left; rfl

/-- **A stale write fences the loop at once.** In the fixed loop, any event
whose store calls include a `StaleWrite` leaves the loop fenced and stopped. -/
theorem stale_write_fences_fixed {s : St α} (e : Ev α)
    (hst : ∃ c ∈ (stepFixed P st s e).calls.drop s.calls.length, c.2 = .fail .staleWrite) :
    (stepFixed P st s e).fenced = true ∧ (stepFixed P st s e).running = false := by
  obtain ⟨c, hc, hcs⟩ := hst
  have nocall : (stepFixed P st s e).calls = s.calls → False := by
    intro h
    rw [h, List.drop_length] at hc
    cases hc
  cases e with
  | write m => exact (nocall rfl).elim
  | fence => exact (nocall rfl).elim
  | wake w now er =>
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      have htick : stepFixed P st s (.wake .tick now er) =
          checkFence (cycleFixed P st true now er s) := by
        simp [stepFixed, iterFixed, hr']
      have hpoll : stepFixed P st s (.wake .poll now er) =
          checkFence (cycleFixed P st false now er s) := by
        simp [stepFixed, iterFixed, hr']
      have main : ∀ tick,
          (∃ c ∈ (checkFence (cycleFixed P st tick now er s)).calls.drop s.calls.length,
            c.2 = .fail .staleWrite) →
          (checkFence (cycleFixed P st tick now er s)).fenced = true ∧
          (checkFence (cycleFixed P st tick now er s)).running = false := by
        intro tick ⟨c, hc, hcs⟩
        rw [(checkFence_shape _).2.2] at hc
        rw [cycleFixed_eq] at hc ⊢
        by_cases hf : s.fenced = true
        · simp only [hf, ↓reduceIte, drained, List.drop_length] at hc
          cases hc
        · simp only [hf, Bool.false_eq_true, ↓reduceIte] at hc ⊢
          rcases cycle_calls_or_due (P := P) (st := st) (tick := tick) (now := now)
            (er := erasedFixed P st er s) s with hcalls | hdue
          · rw [hcalls, List.drop_length] at hc
            cases hc
          · rw [cycle_due hdue] at hc ⊢
            rw [(settle_shape _ _ _ _ _).2.2.2.1] at hc
            simp only [List.drop_left'] at hc
            have hc' : c ∈ (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls := by
              simpa using hc
            have hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result =
                some .staleWrite :=
              ladder_terminal_result _ _ _ c hc' .staleWrite (by rw [hcs]; rfl) rfl
            have her : erasedFixed P st er s = true := by simp [erasedFixed, hres]
            rw [her]
            simp [settle, hres, checkFence, dropFenced]
      cases w with
      | stop =>
        refine (nocall ?_).elim
        by_cases hf : s.fenced = true
        · simp [stepFixed, iterFixed, hr', hf, dropFenced]
        · simp [stepFixed, iterFixed, hr', hf, requeue]
      | tick => rw [htick] at hc ⊢; exact main true ⟨c, hc, hcs⟩
      | poll => rw [hpoll] at hc ⊢; exact main false ⟨c, hc, hcs⟩
    · refine (nocall ?_).elim
      have hr' : (!s.running) = true := by simp at hr; simp [hr]
      simp [stepFixed, iterFixed, hr']

/-- **A stale write never leads to a resend.** After any event whose store
calls include a `StaleWrite`, no run of the fixed loop calls the store again:
the batch is not retained, held and re-sent (contrast `stale_write_retained`). -/
theorem stale_write_never_resent {s : St α} (e : Ev α)
    (hst : ∃ c ∈ (stepFixed P st s e).calls.drop s.calls.length, c.2 = .fail .staleWrite)
    (evs : List (Ev α)) :
    (runFixed P st (stepFixed P st s e) evs).calls = (stepFixed P st s e).calls :=
  runFixed_fenced_silent (stale_write_fences_fixed e hst).1 evs

/-- The scenario of `stale_write_retained`, fixed: one call, the drained batch
dropped as a fenced drop, the loop fenced and stopped, nothing held for a
resend. -/
theorem stale_write_dropped_fixed {s : St α} {now : Nat} {er : Bool}
    (hrun : s.running = true) (hf : s.fenced = false) (h : Due P true now s)
    (hs : st (s.pending ++ s.log) s.attempts = .fail .staleWrite) :
    (iterFixed P st .tick now er s).calls = s.calls ++ [(s.pending ++ s.log, .fail .staleWrite)] ∧
    (iterFixed P st .tick now er s).ledger = s.ledger ++ [(.droppedFenced, s.pending ++ s.log)] ∧
    (iterFixed P st .tick now er s).deadLettered = s.deadLettered ∧
    (iterFixed P st .tick now er s).fenced = true ∧
    (iterFixed P st .tick now er s).running = false := by
  have hr' : (!s.running) = false := by simp [hrun]
  have hres := stale_write_one_call (st := st) hs P.retries
  have her : erasedFixed P st er s = true := by simp [erasedFixed, hres]
  have hne : (s.pending ++ s.log).isEmpty = false := h.nonempty
  simp only [iterFixed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleFixed_eq]
  simp only [hf, Bool.false_eq_true, ↓reduceIte]
  rw [cycle_due h, her, hres]
  simp [settle, checkFence, dropFenced, hne]

end FixedFence

/-! ### The heartbeat and the loop together -/

section FixedLease

variable {H : Type} [DecidableEq H] {P : Params} {st : Store α}

/-- **After a takeover the fence latches within one heartbeat, and then no
flush is issued.** Writer `A` captured token `a.token` at row `a`; after any
history the row `b` is no longer `A`'s generation (another writer holds it, it
was released by the operator override, or `A`'s own refresh re-took it at a
later token). The fixed heartbeat's next beat latches (`Lease.beat_latches_iff`),
and from the `.fence` event on the fixed loop never calls the store again. -/
theorem takeover_fences_fixed {released A : H} {a b : Lease.Row H}
    (hsteps : Lease.Steps released a b) (hnot : ¬ (b.holder = A ∧ b.token = a.token))
    (now ttl : Int) (s : St α) (evs : List (Ev α)) :
    Lease.beatLatches A a.token now ttl b = true ∧
    (runFixed P st s (.fence :: evs)).calls = s.calls :=
  ⟨(Lease.beat_latches_iff hsteps now ttl).mpr hnot, no_flush_after_fence_fixed s evs⟩

/-- The operator-override case of `override_refresh_retakes`, fixed: the old
beat misses the re-take, the fixed beat latches, and nothing is flushed
afterwards. -/
theorem override_fences_fixed {released A : H} (a : Lease.Row H) (hA : a.holder = A)
    (hAr : A ≠ released) {now now' : Int} (hle : now ≤ now') (ttl : Int) (s : St α)
    (evs : List (Ev α)) :
    Lease.beatLatchesOld A now' ttl (Lease.release released A now a) = false ∧
    Lease.beatLatches A a.token now' ttl (Lease.release released A now a) = true ∧
    (runFixed P st s (.fence :: evs)).calls = s.calls :=
  ⟨(Lease.override_beat_latches a hA hAr hle ttl).1,
   (Lease.override_beat_latches a hA hAr hle ttl).2, no_flush_after_fence_fixed s evs⟩

end FixedLease

/-! ### `Memory::close` over the fixed loop

`close` checks the fence, stops the loop and joins it, then final-flushes
whatever is left in the graph log; a degraded session is an error, and an
empty log is `Ok`. In the fixed loop the fence can latch *during* that stop:
a flush the loop had in flight comes back `StaleWrite`, the loop latches the
fence and drops `pending`. A close that read the fence only before the stop
then finds an empty log and reports `Ok` over the dropped batch
(`close_ok_over_drop`). Reading it again after the join closes that
(`close_ok_no_drop`). `evs` is what the loop does between the first check and
the join (its in-flight cycle and the stop); `final` is the store for the
final flush. -/

section Close

variable {P : Params} {st : Store α}

/-- Does `close` return `Ok`? `recheck`: it reads the fence again after the
loop has stopped. -/
def closeOk (P : Params) (st : Store α) (recheck : Bool) (s : St α) (evs : List (Ev α))
    (final : Store α) : Bool :=
  if s.fenced then false
  else
    let s' := runFixed P st s evs
    if recheck && s'.fenced then false
    else if s'.degraded then false
    else if s'.log.isEmpty then true
    else (final s'.log s'.attempts).reported.isNone

/-- **A re-checking close never reports `Ok` over a drop.** When it returns
`Ok`, every batch that ever left `pending` was committed or dead-lettered
(and counted, `Good.dead`): none was dropped for the fence or for
degradation. -/
theorem close_ok_no_drop {s : St α} (hg : Good s) (evs : List (Ev α)) (final : Store α)
    (hok : closeOk P st true s evs final = true) :
    ∀ e ∈ (runFixed P st s evs).ledger, e.1 = .committed ∨ e.1 = .deadLettered := by
  have hg' := good_runFixed (P := P) (st := st) hg evs
  have hf : (runFixed P st s evs).fenced = false := by
    cases h : (runFixed P st s evs).fenced
    · rfl
    · simp [closeOk, h] at hok
  have hd : (runFixed P st s evs).degraded = false := by
    cases h : (runFixed P st s evs).degraded
    · rfl
    · simp [closeOk, h, hf] at hok
  intro e he
  cases hfate : e.1 with
  | committed => exact Or.inl rfl
  | deadLettered => exact Or.inr rfl
  | droppedDegraded =>
    have := hg'.degr e he hfate
    rw [hd] at this; cases this
  | droppedFenced =>
    have := hg'.fence e he hfate
    rw [hf] at this; cases this

end Close

end Fixed

/-! ## The two unguarded store calls (#301)

The loop above (`stepFixed`, the Rust after #293) takes `erased` as an input
and leaves `publish_stats` out. Both are store calls: `erased()` reads the
lease row (`refused_as_erased` → `read_lease`) after a ladder that ended in an
error other than a constraint violation, and `publish_stats` calls
`write_flush_stats` after a flush attempt (and, rate-limited, between
attempts). Before #301 neither runs under `FLUSH_ATTEMPT_TIMEOUT` or
`CatchUnwindPoll`, the guard every `store.flush` attempt has. Here each call
is a `Probe`: it returns, panics, or never returns.

* `stepUnguarded` is the loop before #301. A panic unwinds out of the task,
  which dies holding `pending` (`abort`): `degraded()` stays false, nothing is
  counted, and `close` only logs the join error. A call that never returns
  parks the task (`Task.hung`) where no wake-up, the stop included, reaches it.
* `PEv.guard` / `runGuarded` is the loop after #301: both calls run under the
  same timeout and catch-unwind. A lease read that panics or times out counts
  as a read that failed (`Tombstone::Unknown`, not erased), so the batch takes
  the stale-write or retain arm as before; a stats write that panics or times
  out is ignored like a failed one. The guarded loop is the #293 loop
  (`runFixed`) with that `erased` input, so every `Fixed` theorem holds of it. -/

section Unguarded

variable {α : Type}

/-- What a store call outside the flush attempt's guard did: returned with
its answer, panicked, or never returned. -/
inductive Probe (β : Type) where
  | ret (v : β)
  | panic
  | hang
  deriving DecidableEq, Repr

/-- A loop event with the two calls made explicit. `lease` is what
`read_lease` does if this wake-up's `erased()` reads it (`ret b`: it answered,
and `b` says the row is the erasure tombstone); `stats` is what
`write_flush_stats` does if `publish_stats` calls it (`none`: it does not; an
`Err` is `ret ()`, since the loop ignores it). -/
inductive PEv (α : Type) where
  | write (m : α)
  | fence
  | wake (w : Wake) (now : Nat) (lease : Probe Bool) (stats : Option (Probe Unit))
  deriving DecidableEq, Repr

/-- The flush task: the loop's state, and whether it is parked for good inside
a store call that never returns. -/
structure Task (α : Type) where
  st : St α
  hung : Bool
  deriving DecidableEq, Repr

/-- `Due` as a test. -/
def due (P : Params) (tick : Bool) (now : Nat) (s : St α) : Bool :=
  !(s.pending ++ s.log).isEmpty && !s.degraded && decide ((s.pending ++ s.log).length ≤ P.logMax) &&
    !held s.retryAfter now && (tick || decide (P.maxBatch ≤ (s.pending ++ s.log).length))

theorem due_iff {P : Params} {tick : Bool} {now : Nat} {s : St α} :
    due P tick now s = true ↔ Due P tick now s := by
  constructor
  · intro h
    simp only [due, Bool.and_eq_true, Bool.not_eq_eq_eq_not, Bool.not_true, decide_eq_true_eq,
      Bool.or_eq_true] at h
    obtain ⟨⟨⟨⟨h1, h2⟩, h3⟩, h4⟩, h5⟩ := h
    exact ⟨h1, h2, h3, h4, h5⟩
  · intro h
    have h5 : (tick || decide (P.maxBatch ≤ (s.pending ++ s.log).length)) = true := by
      rcases h.tickOrFull with ht | hf
      · simp [ht]
      · simp only [Bool.or_eq_true, decide_eq_true_eq]; exact Or.inr hf
    simp only [due, h.nonempty, h.notDegraded, h.notHeld, h5, decide_eq_true h.withinMax]
    rfl

/-- The state `drain_and_flush` is in when the ladder has returned, before
its `match`: the drained batch in `pending`, the ladder's calls made. -/
def preSettle (P : Params) (st : Store α) (s : St α) : St α :=
  { s with pending := s.pending ++ s.log, log := [], retryAfter := none,
           calls := s.calls ++ (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls,
           attempts := s.attempts +
             (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls.length }

/-- Does this cycle call `erased()`, and so `read_lease`? Only once a ladder
has ended in an error other than a constraint violation, with the fence
unlatched (the arms before it are `Ok`, `Constraint` and
`Err(_) if self.fenced()`). A `StaleWrite` is read for too: the erased arm
comes before the stale-write arm. -/
def readsLease (P : Params) (st : Store α) (tick : Bool) (now : Nat) (s : St α) : Bool :=
  !s.fenced && due P tick now s &&
    match (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result with
    | some .constraint => false
    | some _ => true
    | none => false

theorem cycleFixed_running {P : Params} {st : Store α} {tick : Bool} {now : Nat} {er : Bool}
    (s : St α) : (cycleFixed P st tick now er s).running = s.running := by
  rw [cycleFixed_eq]
  split
  · rfl
  · exact (cycle_shape (P := P) (st := st) (tick := tick) (now := now) (s := s)).2.2.1

/-- Where the read happens, the cycle is the ladder's state settled with the
read's answer. -/
theorem readsLease_settle {P : Params} {st : Store α} {tick : Bool} {now : Nat} {s : St α}
    (h : readsLease P st tick now s = true) (er : Bool) :
    cycleFixed P st tick now er s =
      settleFixed now er (s.pending ++ s.log)
        (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries) (preSettle P st s) := by
  simp only [readsLease, Bool.and_eq_true, Bool.not_eq_eq_eq_not, Bool.not_true] at h
  obtain ⟨⟨hf, hd⟩, _⟩ := h
  rw [cycleFixed_unfenced hf, cycle_due (due_iff.mp hd), settleFixed_eq]
  rfl

theorem cycle_not_due {P : Params} {st : Store α} {tick : Bool} {now : Nat} {s : St α}
    (h : ¬ Due P tick now s) (er er' : Bool) :
    cycle P st tick now er s = cycle P st tick now er' s := by
  unfold cycle
  dsimp only
  by_cases h1 : (s.pending ++ s.log).isEmpty = true
  · rw [ite_eq_left h1, ite_eq_left h1]
  · rw [ite_eq_right h1, ite_eq_right h1]
    by_cases h2 : s.degraded = true
    · rw [ite_eq_left h2, ite_eq_left h2]
    · rw [ite_eq_right h2, ite_eq_right h2]
      by_cases h3 : P.logMax < (s.pending ++ s.log).length
      · rw [ite_eq_left h3, ite_eq_left h3]
      · rw [ite_eq_right h3, ite_eq_right h3]
        by_cases h4 : held s.retryAfter now = true
        · rw [ite_eq_left h4, ite_eq_left h4]
        · rw [ite_eq_right h4, ite_eq_right h4]
          by_cases h5 : (!tick && decide ((s.pending ++ s.log).length < P.maxBatch)) = true
          · rw [ite_eq_left h5, ite_eq_left h5]
          · exfalso
            apply h
            refine ⟨by simpa using h1, by simpa using h2, by omega, by simpa using h4, ?_⟩
            cases tick
            · right
              simp only [Bool.not_false, Bool.true_and, decide_eq_true_eq] at h5
              omega
            · left; rfl

/-- **The model's input is read only where the Rust reads it.** A cycle that
does not call `erased()` does not depend on its answer, so the `false` the
unguarded and guarded loops pass there is arbitrary. -/
theorem cycleFixed_erased_irrelevant {P : Params} {st : Store α} {tick : Bool} {now : Nat}
    {s : St α} (h : readsLease P st tick now s = false) (er er' : Bool) :
    cycleFixed P st tick now er s = cycleFixed P st tick now er' s := by
  by_cases hf : s.fenced = true
  · rw [cycleFixed_fenced hf, cycleFixed_fenced hf]
  · have hf' : s.fenced = false := by simpa using hf
    rw [cycleFixed_unfenced hf', cycleFixed_unfenced hf']
    by_cases hd : Due P tick now s
    · have hdue : due P tick now s = true := due_iff.mpr hd
      simp only [readsLease, hf', hdue, Bool.not_false, Bool.true_and] at h
      rw [cycle_due hd, cycle_due hd]
      cases hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result with
      | none => simp [settle, hres]
      | some e =>
        rw [hres] at h
        cases e <;> simp at h <;> simp [settle, hres]
    · exact cycle_not_due hd _ _

/-- `drain_and_flush` before #301 (the #293 cycle, `cycleFixed`) with
`read_lease` unguarded. A read that answers is `cycleFixed` with that answer.
A panic unwinds out of the task, which dies holding the drained batch
(`abort` of the ladder's state). A read that never returns parks the task
there for good with the batch (`true`: hung). A cycle that does not read is
`cycleFixed`, its `erased` input unused (`cycleFixed_erased_irrelevant`). -/
def cycleUnguarded (P : Params) (st : Store α) (tick : Bool) (now : Nat) (lease : Probe Bool)
    (s : St α) : St α × Bool :=
  match lease with
  | .ret er => (cycleFixed P st tick now er s, false)
  | .panic =>
    if readsLease P st tick now s then (abort (preSettle P st s), false)
    else (cycleFixed P st tick now false s, false)
  | .hang =>
    if readsLease P st tick now s then (preSettle P st s, true)
    else (cycleFixed P st tick now false s, false)

/-- `publish_stats` before #301. It runs only when the cycle left the fence
unlatched (`cycle`'s `if self.fenced() { return; }`); an error is ignored
(best-effort), a panic kills the task with whatever is in `pending`, and a
call that never returns parks it. -/
def statsUnguarded (stats : Option (Probe Unit)) (s : St α) : St α × Bool :=
  if s.fenced then (s, false)
  else match stats with
    | some .panic => (abort s, false)
    | some .hang => (s, true)
    | _ => (s, false)

/-- A tick or poll of the loop before #301: the cycle, then the stats write,
then the top-of-loop fence check. A task that died or hung goes no further. -/
def cycleThen (P : Params) (st : Store α) (tick : Bool) (now : Nat) (lease : Probe Bool)
    (stats : Option (Probe Unit)) (s : St α) : St α × Bool :=
  let c := cycleUnguarded P st tick now lease s
  if c.2 || !c.1.running then c
  else
    let d := statsUnguarded stats c.1
    if d.2 || !d.1.running then d else (checkFence d.1, false)

def iterUnguarded (P : Params) (st : Store α) (w : Wake) (now : Nat) (lease : Probe Bool)
    (stats : Option (Probe Unit)) (s : St α) : St α × Bool :=
  if !s.running then (s, false)
  else match w with
    | .stop => (if s.fenced then dropFenced s else requeue s, false)
    | .tick => cycleThen P st true now lease stats s
    | .poll => cycleThen P st false now lease stats s

/-- The loop before #301. A hung task ignores every wake-up, the stop
included: the `select!` that would serve it is never reached again. Writes
and the heartbeat's latch still land. -/
def stepUnguarded (P : Params) (st : Store α) (t : Task α) : PEv α → Task α
  | .write m => { t with st := { t.st with hist := t.st.hist ++ [m], log := t.st.log ++ [m] } }
  | .fence => { t with st := { t.st with fenced := true } }
  | .wake w now lease stats =>
    if t.hung then t
    else
      let r := iterUnguarded P st w now lease stats t.st
      ⟨r.1, r.2⟩

def runUnguarded (P : Params) (st : Store α) (t : Task α) (evs : List (PEv α)) : Task α :=
  evs.foldl (stepUnguarded P st) t

theorem runUnguarded_cons {P : Params} {st : Store α} (t : Task α) (e : PEv α) (evs : List (PEv α)) :
    runUnguarded P st t (e :: evs) = runUnguarded P st (stepUnguarded P st t e) evs := rfl

/-- `erased()` after #301: the read runs under `FLUSH_ATTEMPT_TIMEOUT` and
`CatchUnwindPoll`, and a panic or a timeout counts as a read that failed
(`Tombstone::Unknown`): not erased. -/
def guardLease : Probe Bool → Bool
  | .ret b => b
  | .panic => false
  | .hang => false

/-- An event as the loop after #301 sees it. The stats write is guarded the
same way, and whatever it does, as an error did before, changes nothing. -/
def PEv.guard : PEv α → Ev α
  | .write m => .write m
  | .fence => .fence
  | .wake w now lease _ => .wake w now (guardLease lease)

/-- An event of the model without the probes, with both calls answering. -/
def Ev.lift : Ev α → PEv α
  | .write m => .write m
  | .fence => .fence
  | .wake w now er => .wake w now (.ret er) none

theorem Ev.lift_guard (e : Ev α) : e.lift.guard = e := by
  cases e <;> rfl

/-- The loop after #301: the #293 loop, with each wake-up's `erased` input
what the guarded read answers. -/
def runGuarded (P : Params) (st : Store α) (s : St α) (evs : List (PEv α)) : St α :=
  runFixed P st s (evs.map PEv.guard)

/-- Every call that is made returns. -/
def PEv.returns : PEv α → Bool
  | .wake _ _ (.ret _) none => true
  | .wake _ _ (.ret _) (some (.ret _)) => true
  | .wake _ _ _ _ => false
  | _ => true

/-! ### The loop before #301 -/

section UnguardedLoop

variable {P : Params} {st : Store α}

/-- **Agreement.** Where both calls return, the loop before #301 is the
guarded loop: the fix changes nothing there. -/
theorem stepUnguarded_agrees {t : Task α} (ht : t.hung = false) {e : PEv α}
    (he : e.returns = true) :
    stepUnguarded P st t e = ⟨stepFixed P st t.st e.guard, false⟩ := by
  obtain ⟨s, hung⟩ := t
  simp only at ht
  subst ht
  cases e with
  | write m => rfl
  | fence => rfl
  | wake w now lease stats =>
    have hs : ∀ (x : St α), x.running = true → x.fenced = false →
        statsUnguarded stats x = (x, false) := by
      intro x _ hf
      unfold statsUnguarded
      cases lease with
      | ret er =>
        cases stats with
        | none => simp [hf]
        | some p => cases p <;> simp_all [PEv.returns]
      | panic => simp [PEv.returns] at he
      | hang => simp [PEv.returns] at he
    cases lease with
    | panic => simp [PEv.returns] at he
    | hang => simp [PEv.returns] at he
    | ret er =>
      simp only [stepUnguarded, Bool.false_eq_true, ↓reduceIte, PEv.guard, guardLease, stepFixed,
        iterUnguarded, iterFixed]
      by_cases hr : s.running = true
      · have hr' : (!s.running) = false := by simp [hr]
        simp only [hr', Bool.false_eq_true, ↓reduceIte]
        have hcyc : ∀ tick, cycleThen P st tick now (.ret er) stats s =
            (checkFence (cycleFixed P st tick now er s), false) := by
          intro tick
          have hrun := cycleFixed_running (P := P) (st := st) (tick := tick) (now := now)
            (er := er) s
          simp only [cycleThen, cycleUnguarded, Bool.false_or]
          rw [hrun, hr]
          simp only [Bool.not_true, Bool.false_eq_true, ↓reduceIte]
          by_cases hf : (cycleFixed P st tick now er s).fenced = true
          · have : statsUnguarded stats (cycleFixed P st tick now er s) =
                (cycleFixed P st tick now er s, false) := by
              simp [statsUnguarded, hf]
            simp [this, hrun, hr]
          · simp [hs _ (by rw [hrun, hr]) (by simpa using hf), hrun, hr]
        cases w with
        | stop => rfl
        | tick => rw [hcyc true]
        | poll => rw [hcyc false]
      · have hr' : (!s.running) = true := by simp at hr; simp [hr]
        simp only [hr', ↓reduceIte]

theorem runUnguarded_agrees {t : Task α} (ht : t.hung = false) (evs : List (PEv α))
    (he : ∀ e ∈ evs, e.returns = true) :
    runUnguarded P st t evs = ⟨runGuarded P st t.st evs, false⟩ := by
  induction evs generalizing t with
  | nil => obtain ⟨s, h⟩ := t; simp only at ht; subst ht; rfl
  | cons e evs ih =>
    rw [runUnguarded_cons, stepUnguarded_agrees ht (he e (List.mem_cons_self ..))]
    rw [ih rfl (fun e' h' => he e' (List.mem_cons_of_mem _ h'))]
    rfl

/-- Losing a non-empty drained batch with the task leaves the history
unaccounted for (`abort_loses`, from a state past the drain). -/
theorem abort_drained_loses {s x : St α} (hg : Good s) (hne : (s.pending ++ s.log).isEmpty = false)
    (hc : x.ledger = s.ledger) (hh : x.hist = s.hist) (hl : x.log = []) :
    (abort x).consumed ++ (abort x).pending ++ (abort x).log ≠ (abort x).hist := by
  have e1 : (abort x).consumed = s.consumed := by simp [abort, St.consumed, hc]
  have e2 : (abort x).pending = [] := rfl
  have e3 : (abort x).log = [] := hl
  have e4 : (abort x).hist = s.hist := hh
  rw [e1, e2, e3, e4, List.append_nil, List.append_nil]
  intro h
  have hlen := congrArg List.length hg.conserve
  rw [h] at hlen
  simp only [List.length_append] at hlen
  have hp : (s.pending ++ s.log) ≠ [] := by simpa using hne
  have := List.length_pos_iff.mpr hp
  simp only [List.length_append] at this
  omega

/-- A due tick whose ladder ended in an error other than a constraint
violation reads the lease. -/
theorem readsLease_of {s : St α} {now : Nat} (hf : s.fenced = false) (h : Due P true now s)
    {e : Err} (hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some e)
    (hne : e ≠ .constraint) : readsLease P st true now s = true := by
  unfold readsLease
  rw [hf, due_iff.mpr h, hres]
  cases e
  all_goals first | rfl | exact absurd rfl hne

/-- The retain arm: `pending` kept, the post-retry hold set. -/
def retain (now : Nat) (r : Run α) (s : St α) : St α :=
  { s with retryAfter := some (now + r.sleeps.sum + retainedBackoff) }

/-- **Before #301, a lease read that panics loses the batch silently.** A due
tick whose ladder fails with anything but a constraint violation reads the
lease; when the read panics, the task dies holding the drained batch. Nothing
is retained, dead-lettered, counted or flagged: `degraded()` and the fence
stay false, the ledger is unchanged, and the history is no longer accounted
for. -/
theorem lease_panic_loses {s : St α} {now : Nat} (hg : Good s) (hrun : s.running = true)
    (hf : s.fenced = false) (h : Due P true now s) {e : Err}
    (hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some e)
    (hne : e ≠ .constraint) (stats : Option (Probe Unit)) :
    stepUnguarded P st ⟨s, false⟩ (.wake .tick now .panic stats) =
      ⟨abort (preSettle P st s), false⟩ ∧
    (abort (preSettle P st s)).running = false ∧
    (abort (preSettle P st s)).degraded = false ∧
    (abort (preSettle P st s)).fenced = false ∧
    (abort (preSettle P st s)).ledger = s.ledger ∧
    (abort (preSettle P st s)).deadLettered = s.deadLettered ∧
    (abort (preSettle P st s)).consumed ++ (abort (preSettle P st s)).pending ++
      (abort (preSettle P st s)).log ≠ (abort (preSettle P st s)).hist := by
  have hreads := readsLease_of (st := st) hf h hres hne
  have hr' : (!s.running) = false := by simp [hrun]
  refine ⟨?_, rfl, h.notDegraded, hf, rfl, rfl, abort_drained_loses hg h.nonempty rfl rfl rfl⟩
  simp [stepUnguarded, iterUnguarded, hr', cycleThen, cycleUnguarded, hreads, abort]

/-- **Before #301, a stats write that panics loses a retained batch
silently.** The ladder fails, the read answers "not erased", the batch is
retained (`exhausted_retained`), and then `write_flush_stats` panics: the
task dies holding it, with the same silence. -/
theorem stats_panic_loses {s : St α} {now : Nat} (hg : Good s) (hrun : s.running = true)
    (hf : s.fenced = false) (h : Due P true now s) {e : Err}
    (hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some e)
    (hne : e ≠ .constraint) (hns : e ≠ .staleWrite) :
    stepUnguarded P st ⟨s, false⟩ (.wake .tick now (.ret false) (some .panic)) =
      ⟨abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s)), false⟩ ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).running = false ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).degraded = false ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).fenced = false ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).ledger = s.ledger ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).deadLettered = s.deadLettered ∧
    (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s))).consumed ++
      (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s))).pending ++
      (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s))).log ≠
      (abort (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s))).hist := by
  have hreads := readsLease_of (st := st) hf h hres hne
  have hset := readsLease_settle hreads false
  have hr' : (!s.running) = false := by simp [hrun]
  have hret : settleFixed now false (s.pending ++ s.log)
      (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries) (preSettle P st s) =
      retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s) := by
    unfold settleFixed
    rw [hres]
    cases e
    all_goals first | rfl | exact absurd rfl hne | exact absurd rfl hns
  have hpf : (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s)).fenced = false := hf
  have hpr : (retain now (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
      (preSettle P st s)).running = true := hrun
  refine ⟨?_, rfl, h.notDegraded, hf, rfl, rfl, abort_drained_loses hg h.nonempty rfl rfl rfl⟩
  simp [stepUnguarded, iterUnguarded, hr', cycleThen, cycleUnguarded, hset, hret, statsUnguarded,
    hpf, hpr, abort]

/-- A hung task stays hung: no event, the stop included, makes it run, exit or
call the store again, and what it holds stays in it. -/
theorem hung_stays {t : Task α} (ht : t.hung = true) (evs : List (PEv α)) :
    (runUnguarded P st t evs).hung = true ∧
    (runUnguarded P st t evs).st.running = t.st.running ∧
    (runUnguarded P st t evs).st.pending = t.st.pending ∧
    (runUnguarded P st t evs).st.calls = t.st.calls := by
  induction evs generalizing t with
  | nil => exact ⟨ht, rfl, rfl, rfl⟩
  | cons e evs ih =>
    rw [runUnguarded_cons]
    have hstep : (stepUnguarded P st t e).hung = true ∧
        (stepUnguarded P st t e).st.running = t.st.running ∧
        (stepUnguarded P st t e).st.pending = t.st.pending ∧
        (stepUnguarded P st t e).st.calls = t.st.calls := by
      cases e with
      | write m => exact ⟨ht, rfl, rfl, rfl⟩
      | fence => exact ⟨ht, rfl, rfl, rfl⟩
      | wake w now lease stats => simp [stepUnguarded, ht]
    obtain ⟨i1, i2, i3, i4⟩ := ih hstep.1
    exact ⟨i1, i2.trans hstep.2.1, i3.trans hstep.2.2.1, i4.trans hstep.2.2.2⟩

/-- **Before #301, a lease read that never returns blocks `stop()`.** In the
scenario of `lease_panic_loses`, a read that never returns parks the task
holding the drained batch, and from then on no event, a stop included, gets
it out: `Memory::close`'s join never returns. -/
theorem lease_hang_blocks_stop {s : St α} {now : Nat} (hrun : s.running = true)
    (hf : s.fenced = false) (h : Due P true now s) {e : Err}
    (hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some e)
    (hne : e ≠ .constraint) (stats : Option (Probe Unit)) (evs : List (PEv α)) :
    stepUnguarded P st ⟨s, false⟩ (.wake .tick now .hang stats) = ⟨preSettle P st s, true⟩ ∧
    (runUnguarded P st ⟨preSettle P st s, true⟩ evs).hung = true ∧
    (runUnguarded P st ⟨preSettle P st s, true⟩ evs).st.running = true ∧
    (runUnguarded P st ⟨preSettle P st s, true⟩ evs).st.pending = s.pending ++ s.log := by
  have hreads := readsLease_of (st := st) hf h hres hne
  have hr' : (!s.running) = false := by simp [hrun]
  obtain ⟨h1, h2, h3, _⟩ := hung_stays (P := P) (st := st) (t := ⟨preSettle P st s, true⟩) rfl evs
  refine ⟨?_, h1, h2.trans hrun, h3⟩
  simp [stepUnguarded, iterUnguarded, hr', cycleThen, cycleUnguarded, hreads]

end UnguardedLoop

/-! ### The loop after #301 -/

section GuardedLoop

variable {P : Params} {st : Store α}

/-- The #293 loop is the guarded loop with every call answering. -/
theorem runGuarded_lift (s : St α) (evs : List (Ev α)) :
    runGuarded P st s (evs.map Ev.lift) = runFixed P st s evs := by
  unfold runGuarded
  rw [List.map_map]
  have : (PEv.guard ∘ Ev.lift : Ev α → Ev α) = id := funext Ev.lift_guard
  rw [this, List.map_id]

/-- **`good_reachable` for the guarded loop**: whatever the two calls do,
conservation, the dead-letter count and "dropped only once degraded or
fenced" hold of every state it reaches. -/
theorem good_reachable_guarded (evs : List (PEv α)) : Good (runGuarded P st St.init evs) :=
  good_reachable_fixed _

/-- **`no_silent_loss` for the guarded loop.** A panicking or hanging lease
read or stats write loses nothing: every batch that left `pending` was
committed, dead-lettered and counted, or dropped with `degraded()` or the
fence set (contrast `lease_panic_loses`, `stats_panic_loses`). -/
theorem no_silent_loss_guarded (evs : List (PEv α)) :
    ∀ e ∈ (runGuarded P st St.init evs).ledger, e.1 = .committed ∨
      (e.1 = .deadLettered ∧ 0 < (runGuarded P st St.init evs).deadLettered) ∨
      (e.1 = .droppedDegraded ∧ (runGuarded P st St.init evs).degraded = true) ∨
      (e.1 = .droppedFenced ∧ (runGuarded P st St.init evs).fenced = true) :=
  no_silent_loss_fixed _

/-- `stopped_accounted` for the guarded loop: once it has exited, every
mutation is in the ledger or back in the graph log. -/
theorem stopped_accounted_guarded (evs : List (PEv α))
    (hr : (runGuarded P st St.init evs).running = false) :
    (runGuarded P st St.init evs).consumed ++ (runGuarded P st St.init evs).log =
      (runGuarded P st St.init evs).hist :=
  stopped_accounted_fixed _ hr

theorem stepFixed_stop_stops (s : St α) (now : Nat) (er : Bool) :
    (stepFixed P st s (.wake .stop now er)).running = false := by
  cases hr : s.running <;> cases hf : s.fenced <;>
    simp [stepFixed, iterFixed, hr, hf, dropFenced, requeue]

/-- **After #301 a stop always ends the loop.** Whatever the lease read and
the stats write did before it, a stop wake-up leaves the guarded loop exited
(contrast `lease_hang_blocks_stop`). This assumes the timeout bounds every
guarded call: a hung call reaches the loop as a read that failed only once
`FLUSH_ATTEMPT_TIMEOUT` has cut it off. -/
theorem stop_exits_guarded (s : St α) (evs : List (PEv α)) (now : Nat) (lease : Probe Bool)
    (stats : Option (Probe Unit)) :
    (runGuarded P st s (evs ++ [.wake .stop now lease stats])).running = false := by
  unfold runGuarded runFixed
  rw [List.map_append, List.foldl_append]
  simp only [List.map_cons, List.map_nil, List.foldl_cons, List.foldl_nil, PEv.guard]
  exact stepFixed_stop_stops (P := P) (st := st) _ _ _

end GuardedLoop

/-! ### `Memory::close` before and after #301 -/

section CloseProbes

variable {P : Params} {st : Store α}

/-- `close` (with its re-check after the join) over the loop before #301:
`none` when it never returns, because the join waits on a hung task. A task
that died is joined with an error that `close` only logs, then the final
drain goes ahead. -/
def closeOkUnguarded (P : Params) (st : Store α) (recheck : Bool) (t : Task α)
    (evs : List (PEv α)) (final : Store α) : Option Bool :=
  if t.st.fenced then some false
  else
    let t' := runUnguarded P st t evs
    if t'.hung then none
    else if recheck && t'.st.fenced then some false
    else if t'.st.degraded then some false
    else if t'.st.log.isEmpty then some true
    else some (final t'.st.log t'.st.attempts).reported.isNone

/-- `close` over the guarded loop. -/
def closeOkGuarded (P : Params) (st : Store α) (recheck : Bool) (s : St α)
    (evs : List (PEv α)) (final : Store α) : Bool :=
  closeOk P st recheck s (evs.map PEv.guard) final

/-- Where every call returns, the two closes agree. -/
theorem closeOkUnguarded_agrees {t : Task α} (ht : t.hung = false) (r : Bool)
    (evs : List (PEv α)) (he : ∀ e ∈ evs, e.returns = true) (final : Store α) :
    closeOkUnguarded P st r t evs final = some (closeOkGuarded P st r t.st evs final) := by
  unfold closeOkUnguarded closeOkGuarded closeOk
  rw [runUnguarded_agrees ht evs he]
  unfold runGuarded
  generalize runFixed P st t.st (evs.map PEv.guard) = x
  cases hfen : t.st.fenced <;> cases r <;> cases hxf : x.fenced <;> cases hxd : x.degraded <;>
    cases hxl : x.log.isEmpty <;> simp [hxf, hxd, hxl]

/-- **`close_ok_no_drop` for the guarded loop.** A re-checking close over the
guarded loop returns `Ok` only when every batch that left `pending` was
committed or dead-lettered. -/
theorem close_ok_no_drop_guarded {s : St α} (hg : Good s) (evs : List (PEv α)) (final : Store α)
    (hok : closeOkGuarded P st true s evs final = true) :
    ∀ e ∈ (runGuarded P st s evs).ledger, e.1 = .committed ∨ e.1 = .deadLettered :=
  close_ok_no_drop hg _ final hok

/-- **And nothing is missing from the ledger.** When the guarded loop has
exited and close returns `Ok`, every mutation ever written is in the ledger
as committed or dead-lettered, or in the log the final flush sent (contrast
`close_ok_over_panic`, where the unguarded close returns `Ok` over a mutation
that is in neither). -/
theorem close_ok_accounted_guarded {s : St α} (hg : Good s) (evs : List (PEv α))
    (final : Store α) (hok : closeOkGuarded P st true s evs final = true)
    (hstop : (runGuarded P st s evs).running = false) :
    (∀ e ∈ (runGuarded P st s evs).ledger, e.1 = .committed ∨ e.1 = .deadLettered) ∧
    (runGuarded P st s evs).consumed ++ (runGuarded P st s evs).log = (runGuarded P st s evs).hist :=
  ⟨close_ok_no_drop_guarded hg evs final hok,
   stopped_accounted (good_runFixed hg _) hstop⟩

end CloseProbes

end Unguarded

/-! ## Error classes (#301)

What the flush loop learns from a failed flush is the `StoreError` the adapter
built from the driver's error (`map_write_err`, `src/store/error.rs`), and the
adapters' commit-time fence. `Cls` is that classification: a transient
(`Retryable`: SQLSTATE `40xxx`, `08xxx`, `57xxx`, `SQLITE_BUSY` /
`SQLITE_LOCKED`, any error without a code such as a pool timeout or a lost
connection, and any code not known to be permanent; a timed-out or panicking
attempt reaches the loop the same way), a constraint violation, a stale
fencing token, and a permanent error that is not a constraint violation (for
example SQLSTATE `42P01`, undefined table). `CStore` is a store described by
class, and `CStore.via m` is what the loop sees when the adapters map class
`c` to `m c`.

* Before the fix (`Cls.errOld`), `map_write_err` maps everything but a
  constraint violation to `Backend`, so a permanent error is retried, and once
  the retries run out the batch is retained and resent every
  `RETAINED_BACKOFF` until the session degrades on `log_max`
  (`permanent_retried_retained`, on `iterFixed`, the loop as it is now).
* After it (`Cls.errFixed`), a permanent error is `StoreError::Permanent`,
  which is not retryable (`permanent_one_call`), and `drain_and_flush` has an
  arm for it (`settleClassed`): unless the session was erased (that arm comes
  first, so an erased session still fences), the session degrades at once,
  with the batch still in `pending`, so the next cycle drops it as degraded
  (`permanent_degrades_classed`) and `close` returns an error. From then on
  the store is never called again (`permanent_never_resent_classed`).
* Everything else is unchanged: a transient is retried as before
  (`transient_retryable_fixed`, `via_fixed_agrees`), and where the ladder does
  not end in `Permanent` the fixed loop is the loop before
  (`stepClassed_agrees`; `runClassed_old`: over the old classification it is
  the loop before, everywhere). The invariants and the close theorems carry
  over: `good_reachable_classed`, `no_silent_loss_classed`,
  `stopped_accounted_classed`, `close_ok_no_drop_classed` and
  `close_ok_accounted_classed`, over the guarded events of #301 item 2.

**Why degrade and not dead-letter.** The other conservative candidate,
dead-lettering a permanent error like a constraint violation, is
`Cls.errDeadLetter` with the loop unchanged. A permanent error is often a
property of the store, not of the batch (a missing table, a revoked
privilege): then every batch fails the same way. Dead-lettering drops each
one, counts it, keeps the session going, and `close` returns `Ok` with nothing
committed (the `exGone` examples): every acknowledged write lost behind one
counter. Degrading keeps the writes in the in-memory graph, stops store I/O,
and makes `close` an error, so the operator learns that the tail was not
written. It is also where the code before the fix ends up for a permanent
error, after a retry storm and a `log_max` of backlog; the fix gets there on
the first refusal. The cost is the batch-specific permanent error (a value the
store rejects, `exBadValue`): dead-lettering would drop that batch and keep
the session durable, degrading ends durability for the rest of the session.
The split by scope below (`Cause`) removes that cost: a value-scoped error is
dead-lettered, and only a store-scoped one degrades. -/

section Classes

variable {α : Type}

/-- The class of a failed flush, as the adapters decide it (`ErrorClass`)
plus the fencing refusal. -/
inductive Cls where
  | retryable
  | constraint
  | stale
  | permanent
  deriving DecidableEq, Repr

/-- Before #301: `map_write_err` maps everything but a constraint violation
to `Backend`. -/
def Cls.errOld : Cls → Err
  | .retryable => .backend
  | .constraint => .constraint
  | .stale => .staleWrite
  | .permanent => .backend

/-- After #301: a permanent error is `StoreError::Permanent`. -/
def Cls.errFixed : Cls → Err
  | .permanent => .permanent
  | c => c.errOld

/-- The candidate not taken: dead-letter a permanent error as a constraint
violation is dead-lettered. -/
def Cls.errDeadLetter : Cls → Err
  | .permanent => .constraint
  | c => c.errOld

/-- What one flush attempt did, by class. -/
inductive Res where
  | ok
  | fail (c : Cls)
  | lostAck
  deriving DecidableEq, Repr

def Res.outcome (m : Cls → Err) : Res → Outcome
  | .ok => .ok
  | .fail c => .fail (m c)
  | .lostAck => .lostAck

/-- A store described by class. -/
abbrev CStore (α : Type) := List α → Nat → Res

/-- The store as the loop sees it when class `c` reaches it as `m c`. -/
def CStore.via (m : Cls → Err) (cs : CStore α) : Store α := fun b n => (cs b n).outcome m

/-- **The bug.** Before #301 a permanent error reaches the loop retryable. -/
theorem permanent_retryable_old : (Cls.errOld .permanent).retryable = true := rfl

/-- After #301 it does not. -/
theorem permanent_not_retryable_fixed : (Cls.errFixed .permanent).retryable = false := rfl

/-- **A transient stays retryable**, and every class but `permanent` reaches
the loop as it did before. -/
theorem transient_retryable_fixed : (Cls.errFixed .retryable).retryable = true := rfl

theorem errFixed_agrees {c : Cls} (h : c ≠ .permanent) : c.errFixed = c.errOld := by
  cases c <;> simp_all [Cls.errFixed]

/-- A store that never fails permanently is seen as before. -/
theorem via_fixed_agrees {cs : CStore α} (h : ∀ b n, cs b n ≠ .fail .permanent) :
    CStore.via Cls.errFixed cs = CStore.via Cls.errOld cs := by
  funext b n
  have hb := h b n
  simp only [CStore.via]
  cases hc : cs b n with
  | ok => rfl
  | lostAck => rfl
  | fail c =>
    rw [hc] at hb
    have : c ≠ .permanent := fun e => hb (by rw [e])
    simp [Res.outcome, errFixed_agrees this]

/-- No error the old classification produces is `permanent`. -/
theorem via_old_not_permanent (cs : CStore α) (b : List α) (n : Nat) :
    (CStore.via Cls.errOld cs b n).reported ≠ some .permanent := by
  simp only [CStore.via]
  cases cs b n with
  | ok => simp [Res.outcome, Outcome.reported]
  | lostAck => simp [Res.outcome, Outcome.reported]
  | fail c => cases c <;> simp [Res.outcome, Outcome.reported, Cls.errOld]

/-- A ladder against a store that fails every attempt on `b` with the same
retryable error makes `retries + 1` identical calls. -/
theorem ladder_always {st : Store α} {b : List α} {e : Err} (hs : ∀ n, st b n = .fail e)
    (hr : e.retryable = true) (n bo k : Nat) :
    (ladder st b n bo k).calls = List.replicate (k + 1) (b, .fail e) ∧
    (ladder st b n bo k).result = some e := by
  induction k generalizing n bo with
  | zero => simp [ladder, hs, Outcome.reported]
  | succ k ih =>
    have h : (st b n).reported = some e := by rw [hs]; rfl
    rw [ladder_retry h hr]
    obtain ⟨h1, h2⟩ := ih (n + 1) (nextBackoff bo)
    refine ⟨?_, h2⟩
    simp only [h1, hs, List.replicate_succ]

/-- The ladder's result is what some attempt reported. -/
theorem ladder_result_mem {st : Store α} {b : List α} (n bo k : Nat) {e : Err}
    (h : (ladder st b n bo k).result = some e) : ∃ m, (st b m).reported = some e := by
  obtain ⟨c, hc, hres⟩ := ladder_result_last (st := st) (b := b) n bo k
  have hmem : c ∈ (ladder st b n bo k).calls := List.mem_of_getLast? hc
  obtain ⟨i, hi, hci⟩ := List.getElem_of_mem hmem
  have hidx := ladder_attempt_index (st := st) (b := b) n bo k i hi
  refine ⟨n + i, ?_⟩
  rw [← hidx, hci, ← hres, h]

/-- **Before the fix: retried, then retained.** In the loop as it is now
(`iterFixed`), a store that refuses the batch with a permanent error is called
`retries + 1` times with it, each refusal seen as `Backend`; then the batch is
retained, nothing is dropped or counted, the session does not degrade, and
the post-retry hold is set, after which `retained_resent` sends it again.
This holds until the backlog passes `log_max`. -/
theorem permanent_retried_retained {P : Params} {cs : CStore α} {s : St α} {now : Nat}
    (hrun : s.running = true) (hf : s.fenced = false) (h : Due P true now s)
    (hp : ∀ n, cs (s.pending ++ s.log) n = .fail .permanent) :
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).calls =
      s.calls ++ List.replicate (P.retries + 1) (s.pending ++ s.log, .fail .backend) ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).pending = s.pending ++ s.log ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).ledger = s.ledger ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).deadLettered = s.deadLettered ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).degraded = false ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).running = true ∧
    (iterFixed P (CStore.via Cls.errOld cs) .tick now false s).retryAfter =
      some (now + (flushWithRetry (CStore.via Cls.errOld cs) (s.pending ++ s.log) s.attempts
        P.retries).sleeps.sum + retainedBackoff) := by
  have hs : ∀ n, CStore.via Cls.errOld cs (s.pending ++ s.log) n = .fail .backend := by
    intro n; simp [CStore.via, hp, Res.outcome, Cls.errOld]
  obtain ⟨hcalls, hres⟩ := ladder_always hs rfl s.attempts backoffBase P.retries
  have her : erasedFixed P (CStore.via Cls.errOld cs) false s = false := by
    simp [erasedFixed, flushWithRetry, hres]
  have hr' : (!s.running) = false := by simp [hrun]
  simp only [iterFixed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleFixed_unfenced hf, her, cycle_due h]
  simp only [flushWithRetry] at hcalls hres ⊢
  simp [settle, hres, hcalls, checkFence, hf, hrun, h.notDegraded]

/-! ### The loop after the fix -/

/-- `drain_and_flush`'s `match` after #301. A `Permanent` error degrades the
session; the erased arm comes before it, so an erased session's permanent
error fences as before. Everything else is `settleFixed`. -/
def settleClassed (now : Nat) (erased : Bool) (p : List α) (r : Run α) (s : St α) : St α :=
  match r.result with
  | some .permanent => if erased then { s with fenced := true } else { s with degraded := true }
  | _ => settleFixed now erased p r s

/-- `drain_and_flush` after #301 item 3: `cycleFixed` with `settleClassed`. -/
def cycleClassed (P : Params) (st : Store α) (tick : Bool) (now : Nat) (erased : Bool)
    (s0 : St α) : St α :=
  let p := s0.pending ++ s0.log
  let s := { s0 with pending := p, log := [] }
  if s.fenced then s
  else if p.isEmpty then s
  else if s.degraded then { s with pending := [], ledger := s.ledger ++ [(.droppedDegraded, p)] }
  else if P.logMax < p.length then { s with degraded := true }
  else if held s.retryAfter now then s
  else
    let s := { s with retryAfter := none }
    if !tick && decide (p.length < P.maxBatch) then s
    else
      let r := flushWithRetry st p s.attempts P.retries
      settleClassed now erased p r
        { s with calls := s.calls ++ r.calls, attempts := s.attempts + r.calls.length }

def iterClassed (P : Params) (st : Store α) (w : Wake) (now : Nat) (erased : Bool) (s : St α) :
    St α :=
  if !s.running then s
  else match w with
    | .stop => if s.fenced then dropFenced s else requeue s
    | .tick => checkFence (cycleClassed P st true now erased s)
    | .poll => checkFence (cycleClassed P st false now erased s)

def stepClassed (P : Params) (st : Store α) (s : St α) : Ev α → St α
  | .write m => { s with hist := s.hist ++ [m], log := s.log ++ [m] }
  | .fence => { s with fenced := true }
  | .wake w now erased => iterClassed P st w now erased s

def runClassed (P : Params) (st : Store α) (s : St α) (evs : List (Ev α)) : St α :=
  evs.foldl (stepClassed P st) s

theorem runClassed_cons {P : Params} {st : Store α} (s : St α) (e : Ev α) (evs : List (Ev α)) :
    runClassed P st s (e :: evs) = runClassed P st (stepClassed P st s e) evs := rfl

/-- The fixed loop with the probes of #301 item 2 guarded: the Rust after
#301. -/
def runClassedGuarded (P : Params) (st : Store α) (s : St α) (evs : List (PEv α)) : St α :=
  runClassed P st s (evs.map PEv.guard)

/-! ### The fixed cycle is `cycleFixed` but for the permanent arm -/

/-- The shape `cycleFixed` and `cycleClassed` share; they differ only in the
settle. -/
def cycleWith (settleFn : Nat → Bool → List α → Run α → St α → St α) (P : Params)
    (st : Store α) (tick : Bool) (now : Nat) (erased : Bool) (s0 : St α) : St α :=
  let p := s0.pending ++ s0.log
  let s := { s0 with pending := p, log := [] }
  if s.fenced then s
  else if p.isEmpty then s
  else if s.degraded then { s with pending := [], ledger := s.ledger ++ [(.droppedDegraded, p)] }
  else if P.logMax < p.length then { s with degraded := true }
  else if held s.retryAfter now then s
  else
    let s := { s with retryAfter := none }
    if !tick && decide (p.length < P.maxBatch) then s
    else
      let r := flushWithRetry st p s.attempts P.retries
      settleFn now erased p r
        { s with calls := s.calls ++ r.calls, attempts := s.attempts + r.calls.length }

section ClassedCycle

variable {P : Params} {st : Store α} {tick : Bool} {now : Nat} {er : Bool} {s : St α}

theorem cycleFixed_with : cycleFixed P st tick now er s = cycleWith settleFixed P st tick now er s :=
  rfl

theorem cycleClassed_with :
    cycleClassed P st tick now er s = cycleWith settleClassed P st tick now er s := rfl

theorem cycleWith_due (f : Nat → Bool → List α → Run α → St α → St α) (hf : s.fenced = false)
    (h : Due P tick now s) :
    cycleWith f P st tick now er s =
      f now er (s.pending ++ s.log) (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries)
        (preSettle P st s) := by
  have h1 := h.nonempty
  have h2 := h.notDegraded
  have h3 : ¬ P.logMax < (s.pending ++ s.log).length := Nat.not_lt.mpr h.withinMax
  have h4 := h.notHeld
  have h5 : (!tick && decide ((s.pending ++ s.log).length < P.maxBatch)) = false := by
    rcases h.tickOrFull with ht | hfull
    · simp [ht]
    · cases tick
      · simp only [Bool.not_false, Bool.true_and, decide_eq_false_iff_not, Nat.not_lt]
        exact hfull
      · simp
  unfold cycleWith
  simp only [preSettle, hf, h1, h2, h3, h4, h5, Bool.false_eq_true, ↓reduceIte]

theorem cycleWith_not_due (f g : Nat → Bool → List α → Run α → St α → St α)
    (h : ¬ (s.fenced = false ∧ Due P tick now s)) :
    cycleWith f P st tick now er s = cycleWith g P st tick now er s := by
  unfold cycleWith
  dsimp only
  by_cases h0 : s.fenced = true
  · rw [ite_eq_left h0, ite_eq_left h0]
  · rw [ite_eq_right h0, ite_eq_right h0]
    by_cases h1 : (s.pending ++ s.log).isEmpty = true
    · rw [ite_eq_left h1, ite_eq_left h1]
    · rw [ite_eq_right h1, ite_eq_right h1]
      by_cases h2 : s.degraded = true
      · rw [ite_eq_left h2, ite_eq_left h2]
      · rw [ite_eq_right h2, ite_eq_right h2]
        by_cases h3 : P.logMax < (s.pending ++ s.log).length
        · rw [ite_eq_left h3, ite_eq_left h3]
        · rw [ite_eq_right h3, ite_eq_right h3]
          by_cases h4 : held s.retryAfter now = true
          · rw [ite_eq_left h4, ite_eq_left h4]
          · rw [ite_eq_right h4, ite_eq_right h4]
            by_cases h5 : (!tick && decide ((s.pending ++ s.log).length < P.maxBatch)) = true
            · rw [ite_eq_left h5, ite_eq_left h5]
            · exfalso
              apply h
              refine ⟨by simpa using h0, by simpa using h1, by simpa using h2, by omega,
                by simpa using h4, ?_⟩
              cases tick
              · right
                simp only [Bool.not_false, Bool.true_and, decide_eq_true_eq] at h5
                omega
              · left; rfl

/-- A cycle that does not reach the store makes no call. -/
theorem cycleClassed_calls_not_due (h : ¬ (s.fenced = false ∧ Due P tick now s)) :
    (cycleClassed P st tick now er s).calls = s.calls := by
  rw [cycleClassed_with, cycleWith_not_due settleClassed settleFixed h, ← cycleFixed_with]
  by_cases hf : s.fenced = true
  · rw [cycleFixed_fenced hf]; rfl
  · have hf' : s.fenced = false := by simpa using hf
    rw [cycleFixed_unfenced hf']
    rcases cycle_calls_or_due (P := P) (st := st) (tick := tick) (now := now)
      (er := erasedFixed P st er s) s with hc | hd
    · exact hc
    · exact absurd ⟨hf', hd⟩ h

theorem settleClassed_eq (now : Nat) (er : Bool) (p : List α) (r : Run α) (s : St α) :
    settleClassed now er p r s =
      if r.result = some .permanent ∧ er = false then { s with degraded := true }
      else settleFixed now er p r s := by
  unfold settleClassed
  cases h : r.result with
  | none => simp
  | some e => cases e <;> cases er <;> simp [settleFixed, h]

theorem settleClassed_calls (now : Nat) (er : Bool) (p : List α) (r : Run α) (x : St α) :
    (settleClassed now er p r x).calls = x.calls := by
  rw [settleClassed_eq]
  split
  · rfl
  · rw [settleFixed_eq]; exact (settle_shape _ _ _ _ _).2.2.2.1

/-- **The fixed cycle is the cycle before, but for one case**: a due cycle
whose ladder ends in `Permanent`, in a session not erased, degrades with the
drained batch still in `pending`. -/
theorem cycleClassed_cases :
    cycleClassed P st tick now er s = cycleFixed P st tick now er s ∨
    (cycleClassed P st tick now er s = { preSettle P st s with degraded := true } ∧
      s.fenced = false ∧ Due P tick now s ∧
      (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result = some .permanent ∧
      er = false) := by
  rw [cycleClassed_with, cycleFixed_with]
  by_cases hd : s.fenced = false ∧ Due P tick now s
  · rw [cycleWith_due _ hd.1 hd.2, cycleWith_due _ hd.1 hd.2, settleClassed_eq]
    split
    · rename_i hc
      exact Or.inr ⟨rfl, hd.1, hd.2, hc.1, hc.2⟩
    · exact Or.inl rfl
  · exact Or.inl (cycleWith_not_due _ _ hd)

end ClassedCycle

/-! ### Agreement -/

section ClassedAgree

variable {P : Params} {st : Store α}

/-- **Agreement.** In a state whose next ladder would not end in `Permanent`,
every event does what it did before the fix. -/
theorem stepClassed_agrees {s : St α}
    (hs : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result ≠
      some .permanent) (e : Ev α) :
    stepClassed P st s e = stepFixed P st s e := by
  have hc : ∀ tick now er, cycleClassed P st tick now er s = cycleFixed P st tick now er s := by
    intro tick now er
    rcases cycleClassed_cases (P := P) (st := st) (tick := tick) (now := now) (er := er)
      (s := s) with h | ⟨_, _, _, h, _⟩
    · exact h
    · exact absurd h hs
  cases e with
  | write m => rfl
  | fence => rfl
  | wake w now er =>
    simp only [stepClassed, stepFixed, iterClassed, iterFixed]
    cases w with
    | stop => rfl
    | tick => simp only [hc]
    | poll => simp only [hc]

/-- **The new arm alone changes nothing**: over the old classification, which
never produces `Permanent`, the fixed loop is the loop before. The fix is the
classification and the arm together. -/
theorem runClassed_old (cs : CStore α) (s : St α) (evs : List (Ev α)) :
    runClassed P (CStore.via Cls.errOld cs) s evs = runFixed P (CStore.via Cls.errOld cs) s evs := by
  induction evs generalizing s with
  | nil => rfl
  | cons e evs ih =>
    rw [runClassed_cons, runFixed_cons]
    have hs : (flushWithRetry (CStore.via Cls.errOld cs) (s.pending ++ s.log) s.attempts
        P.retries).result ≠ some .permanent := by
      intro h
      obtain ⟨m, hm⟩ := ladder_result_mem _ _ _ h
      exact via_old_not_permanent cs _ m hm
    rw [stepClassed_agrees hs, ih]

end ClassedAgree

/-! ### A permanent error is never retried -/

section ClassedPermanent

variable {P : Params} {st : Store α}

/-- **One call.** A `Permanent` error ends the ladder at once, with no
backoff, whatever `retries` is. -/
theorem permanent_one_call {b : List α} {n : Nat} (hs : st b n = .fail .permanent)
    (retries : Nat) :
    flushWithRetry st b n retries = ⟨[(b, .fail .permanent)], [], some .permanent⟩ := by
  have hrep : (st b n).reported = some .permanent := by rw [hs]; rfl
  unfold flushWithRetry
  rw [ladder_nonretryable hrep rfl, hs]

/-- **The issue's scenario, fixed.** The store refuses the batch with a
permanent error: one call, then the session degrades, holding the batch in
`pending` for the degraded drop; nothing is dead-lettered or held for a
resend (contrast `permanent_retried_retained`). -/
theorem permanent_degrades_classed {cs : CStore α} {s : St α} {now : Nat}
    (hrun : s.running = true) (hf : s.fenced = false) (h : Due P true now s)
    (hp : cs (s.pending ++ s.log) s.attempts = .fail .permanent) :
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).calls =
      s.calls ++ [(s.pending ++ s.log, .fail .permanent)] ∧
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).pending = s.pending ++ s.log ∧
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).ledger = s.ledger ∧
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).deadLettered = s.deadLettered ∧
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).degraded = true ∧
    (iterClassed P (CStore.via Cls.errFixed cs) .tick now false s).running = true := by
  have hs : CStore.via Cls.errFixed cs (s.pending ++ s.log) s.attempts = .fail .permanent := by
    simp [CStore.via, hp, Res.outcome, Cls.errFixed]
  have hres := permanent_one_call (st := CStore.via Cls.errFixed cs) hs P.retries
  have hr' : (!s.running) = false := by simp [hrun]
  simp only [iterClassed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleClassed_with, cycleWith_due _ hf h]
  simp [settleClassed, preSettle, checkFence, hf, hrun, hres]

theorem checkFence_degraded (x : St α) : (checkFence x).degraded = x.degraded := by
  unfold checkFence; split <;> rfl

theorem checkFence_fenced (x : St α) : (checkFence x).fenced = x.fenced := by
  unfold checkFence; split <;> simp_all [dropFenced]

/-- Once degraded or fenced, no event of the fixed loop calls the store, and
it stays so. -/
theorem stepClassed_silent {s : St α} (h : s.degraded = true ∨ s.fenced = true) (e : Ev α) :
    (stepClassed P st s e).calls = s.calls ∧
    ((stepClassed P st s e).degraded = true ∨ (stepClassed P st s e).fenced = true) := by
  cases e with
  | write m => exact ⟨rfl, h⟩
  | fence => exact ⟨rfl, Or.inr rfl⟩
  | wake w now er =>
    simp only [stepClassed, iterClassed]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      simp only [hr', Bool.false_eq_true, ↓reduceIte]
      have hnd : ¬ (s.fenced = false ∧ Due P true now s) ∧
          ¬ (s.fenced = false ∧ Due P false now s) := by
        constructor <;>
        · rintro ⟨hf, hd⟩
          rcases h with h | h
          · rw [hd.notDegraded] at h; cases h
          · rw [hf] at h; cases h
      have hcyc : ∀ tick, ¬ (s.fenced = false ∧ Due P tick now s) →
          (checkFence (cycleClassed P st tick now er s)).calls = s.calls ∧
          ((checkFence (cycleClassed P st tick now er s)).degraded = true ∨
            (checkFence (cycleClassed P st tick now er s)).fenced = true) := by
        intro tick hn
        refine ⟨(checkFence_shape _).2.2.trans (cycleClassed_calls_not_due hn), ?_⟩
        rw [checkFence_degraded, checkFence_fenced]
        rcases cycleClassed_cases (P := P) (st := st) (tick := tick) (now := now) (er := er)
          (s := s) with hc | ⟨_, hf, hd, _⟩
        · rw [hc]
          by_cases hf : s.fenced = true
          · rw [cycleFixed_fenced hf]; exact Or.inr hf
          · have hf' : s.fenced = false := by simpa using hf
            rw [cycleFixed_unfenced hf']
            obtain ⟨_, _, _, h4, h5, _⟩ := cycle_shape (P := P) (st := st) (tick := tick)
              (now := now) (er := erasedFixed P st er s) (s := s)
            have hle : ∀ {a b : Bool}, a ≤ b → a = true → b = true := by decide
            rcases h with h | h
            · exact Or.inl (hle h4 h)
            · exact Or.inr (hle h5 h)
        · exact absurd ⟨hf, hd⟩ hn
      cases w with
      | stop =>
        by_cases hf : s.fenced = true
        · simp only [hf, ↓reduceIte]
          exact ⟨rfl, Or.inr hf⟩
        · simp only [hf, Bool.false_eq_true, ↓reduceIte]
          exact ⟨rfl, h⟩
      | tick => exact hcyc true hnd.1
      | poll => exact hcyc false hnd.2
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [ite_eq_left hr']
      exact ⟨rfl, h⟩

theorem runClassed_silent {s : St α} (h : s.degraded = true ∨ s.fenced = true)
    (evs : List (Ev α)) : (runClassed P st s evs).calls = s.calls := by
  induction evs generalizing s with
  | nil => rfl
  | cons e evs ih =>
    rw [runClassed_cons, ih (stepClassed_silent h e).2, (stepClassed_silent h e).1]

/-- After any event whose store calls include a `Permanent` refusal, the
fixed loop is degraded (or fenced, for an erased session). -/
theorem permanent_stops_classed {s : St α} (e : Ev α)
    (hp : ∃ c ∈ (stepClassed P st s e).calls.drop s.calls.length, c.2 = .fail .permanent) :
    (stepClassed P st s e).degraded = true ∨ (stepClassed P st s e).fenced = true := by
  obtain ⟨c, hc, hcp⟩ := hp
  have nocall : (stepClassed P st s e).calls = s.calls → False := by
    intro h
    rw [h, List.drop_length] at hc
    cases hc
  cases e with
  | write m => exact (nocall rfl).elim
  | fence => exact (nocall rfl).elim
  | wake w now er =>
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      have main : ∀ tick,
          (∃ c ∈ (checkFence (cycleClassed P st tick now er s)).calls.drop s.calls.length,
            c.2 = .fail .permanent) →
          (checkFence (cycleClassed P st tick now er s)).degraded = true ∨
          (checkFence (cycleClassed P st tick now er s)).fenced = true := by
        intro tick ⟨c, hc, hcp⟩
        rw [(checkFence_shape _).2.2] at hc
        rw [checkFence_degraded, checkFence_fenced]
        by_cases hd : s.fenced = false ∧ Due P tick now s
        · rw [cycleClassed_with, cycleWith_due _ hd.1 hd.2] at hc ⊢
          rw [settleClassed_calls] at hc
          simp only [preSettle, List.drop_left'] at hc
          have hc' : c ∈ (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).calls := by
            simpa using hc
          have hres : (flushWithRetry st (s.pending ++ s.log) s.attempts P.retries).result =
              some .permanent :=
            ladder_terminal_result _ _ _ c hc' .permanent (by rw [hcp]; rfl) rfl
          cases er
          · left; simp [settleClassed, hres]
          · right; simp [settleClassed, hres]
        · rw [cycleClassed_calls_not_due hd, List.drop_length] at hc
          cases hc
      cases w with
      | stop =>
        refine (nocall ?_).elim
        by_cases hf : s.fenced = true
        · simp [stepClassed, iterClassed, hr', hf, dropFenced]
        · simp [stepClassed, iterClassed, hr', hf, requeue]
      | tick =>
        simp only [stepClassed, iterClassed, hr', Bool.false_eq_true, ↓reduceIte] at hc hcp ⊢
        exact main true ⟨c, hc, hcp⟩
      | poll =>
        simp only [stepClassed, iterClassed, hr', Bool.false_eq_true, ↓reduceIte] at hc hcp ⊢
        exact main false ⟨c, hc, hcp⟩
    · refine (nocall ?_).elim
      have hr' : (!s.running) = true := by simp at hr; simp [hr]
      simp [stepClassed, iterClassed, hr']

/-- **A permanent error is never retried.** Within its ladder it is the last
call (`permanent_one_call`); after the event that saw it, no later event of
the fixed loop calls the store again (contrast `permanent_retried_retained`,
where the old loop resends it every `RETAINED_BACKOFF`). -/
theorem permanent_never_resent_classed {s : St α} (e : Ev α)
    (hp : ∃ c ∈ (stepClassed P st s e).calls.drop s.calls.length, c.2 = .fail .permanent)
    (evs : List (Ev α)) :
    (runClassed P st (stepClassed P st s e) evs).calls = (stepClassed P st s e).calls :=
  runClassed_silent (permanent_stops_classed e hp) evs

end ClassedPermanent

/-! ### The invariants and the close theorems carry over -/

section ClassedInvariants

variable {P : Params} {st : Store α}

theorem good_preSettle_degraded {s : St α} (hg : Good s) (hrun : s.running = true) :
    Good { preSettle P st s with degraded := true } where
  conserve := by
    have hc := hg.conserve
    simp only [preSettle, St.consumed] at hc ⊢
    simpa [List.append_assoc] using hc
  dead := hg.dead
  degr := by intro _ _ _; rfl
  fence := hg.fence
  stopped := by intro h; simp [preSettle, hrun] at h

theorem good_cycleClassed {tick : Bool} {now : Nat} {er : Bool} {s : St α} (hg : Good s)
    (hrun : s.running = true) : Good (cycleClassed P st tick now er s) := by
  rcases cycleClassed_cases (P := P) (st := st) (tick := tick) (now := now) (er := er) (s := s)
    with h | ⟨h, _⟩
  · rw [h]; exact good_cycleFixed hg hrun
  · rw [h]; exact good_preSettle_degraded hg hrun

theorem good_stepClassed {s : St α} (hg : Good s) (e : Ev α) : Good (stepClassed P st s e) := by
  cases e with
  | write m => exact good_step (P := P) (st := st) hg (.write m)
  | fence => exact good_step (P := P) (st := st) hg .fence
  | wake w now er =>
    simp only [stepClassed, iterClassed]
    by_cases hr : s.running = true
    · have hr' : (!s.running) = false := by simp [hr]
      rw [hr']
      simp only [Bool.false_eq_true, ↓reduceIte]
      cases w with
      | stop =>
        by_cases hf : s.fenced = true
        · simp only [hf, ↓reduceIte]; exact good_dropFenced hg hf
        · simp only [hf, Bool.false_eq_true, ↓reduceIte]; exact good_requeue hg
      | tick => exact good_checkFence (good_cycleClassed hg hr)
      | poll => exact good_checkFence (good_cycleClassed hg hr)
    · have hr' : (!s.running) = true := by simp at hr; simp [hr]
      rw [hr']
      exact hg

theorem good_runClassed {s : St α} (hg : Good s) (evs : List (Ev α)) :
    Good (runClassed P st s evs) := by
  induction evs generalizing s with
  | nil => exact hg
  | cons e evs ih => exact ih (good_stepClassed hg e)

/-- **`good_reachable` after #301 item 3**, over the guarded events of item
2: conservation, the dead-letter count and "dropped only once degraded or
fenced" hold of every state. -/
theorem good_reachable_classed (evs : List (PEv α)) :
    Good (runClassedGuarded P st St.init evs) :=
  good_runClassed good_init _

/-- **`no_silent_loss` after #301 item 3.** A batch refused with a permanent
error is dropped only as a degraded drop, with `degraded()` set: the session
is reported as degraded, never quietly emptied. -/
theorem no_silent_loss_classed (evs : List (PEv α)) :
    ∀ e ∈ (runClassedGuarded P st St.init evs).ledger, e.1 = .committed ∨
      (e.1 = .deadLettered ∧ 0 < (runClassedGuarded P st St.init evs).deadLettered) ∨
      (e.1 = .droppedDegraded ∧ (runClassedGuarded P st St.init evs).degraded = true) ∨
      (e.1 = .droppedFenced ∧ (runClassedGuarded P st St.init evs).fenced = true) :=
  no_silent_loss (good_reachable_classed evs)

/-- `stopped_accounted` after #301 item 3. -/
theorem stopped_accounted_classed (evs : List (PEv α))
    (hr : (runClassedGuarded P st St.init evs).running = false) :
    (runClassedGuarded P st St.init evs).consumed ++ (runClassedGuarded P st St.init evs).log =
      (runClassedGuarded P st St.init evs).hist :=
  stopped_accounted (good_reachable_classed evs) hr

/-- `closeOk` over the fixed loop. -/
def closeOkClassed (P : Params) (st : Store α) (recheck : Bool) (s : St α) (evs : List (Ev α))
    (final : Store α) : Bool :=
  if s.fenced then false
  else
    let s' := runClassed P st s evs
    if recheck && s'.fenced then false
    else if s'.degraded then false
    else if s'.log.isEmpty then true
    else (final s'.log s'.attempts).reported.isNone

/-- `close` over the fixed loop with the guarded events of #301 item 2. -/
def closeOkClassedGuarded (P : Params) (st : Store α) (recheck : Bool) (s : St α)
    (evs : List (PEv α)) (final : Store α) : Bool :=
  closeOkClassed P st recheck s (evs.map PEv.guard) final

/-- **A degraded session never closes `Ok`**, so a permanent refusal always
reaches the caller of `close` as an error. -/
theorem close_not_ok_degraded {s : St α} (evs : List (Ev α)) (r : Bool) (final : Store α)
    (hd : (runClassed P st s evs).degraded = true) :
    closeOkClassed P st r s evs final = false := by
  unfold closeOkClassed
  cases hf : s.fenced <;> cases r <;> cases hxf : (runClassed P st s evs).fenced <;>
    simp [hd, hxf]

/-- **`close_ok_no_drop` after #301 item 3.** -/
theorem close_ok_no_drop_classed {s : St α} (hg : Good s) (evs : List (Ev α)) (final : Store α)
    (hok : closeOkClassed P st true s evs final = true) :
    ∀ e ∈ (runClassed P st s evs).ledger, e.1 = .committed ∨ e.1 = .deadLettered := by
  have hg' := good_runClassed (P := P) (st := st) hg evs
  have hf : (runClassed P st s evs).fenced = false := by
    cases h : (runClassed P st s evs).fenced
    · rfl
    · simp [closeOkClassed, h] at hok
  have hd : (runClassed P st s evs).degraded = false := by
    cases h : (runClassed P st s evs).degraded
    · rfl
    · simp [closeOkClassed, h, hf] at hok
  intro e he
  cases hfate : e.1 with
  | committed => exact Or.inl rfl
  | deadLettered => exact Or.inr rfl
  | droppedDegraded =>
    have := hg'.degr e he hfate
    rw [hd] at this; cases this
  | droppedFenced =>
    have := hg'.fence e he hfate
    rw [hf] at this; cases this

/-- **`close_ok_accounted_guarded` after #301 item 3.** When the loop has
exited and close returns `Ok`, every mutation ever written is committed,
dead-lettered, or in the log the final flush sent. -/
theorem close_ok_accounted_classed {s : St α} (hg : Good s) (evs : List (PEv α))
    (final : Store α) (hok : closeOkClassedGuarded P st true s evs final = true)
    (hstop : (runClassedGuarded P st s evs).running = false) :
    (∀ e ∈ (runClassedGuarded P st s evs).ledger, e.1 = .committed ∨ e.1 = .deadLettered) ∧
    (runClassedGuarded P st s evs).consumed ++ (runClassedGuarded P st s evs).log =
      (runClassedGuarded P st s evs).hist :=
  ⟨close_ok_no_drop_classed hg _ final hok, stopped_accounted (good_runClassed hg _) hstop⟩

end ClassedInvariants

/-! ### Value-scoped and store-scoped permanent errors (#301 review)

The fix above treats every permanent error alike and degrades. `exBadValue`
shows what that costs when the error belongs to the batch: degrading drops
that batch too, as a dead letter would, and also ends durability for the
rest of the session. So the remediated fix splits the class by scope
(`Cause`): an error about a value the store rejects (SQLSTATE classes `21`,
`22`, `44`, `54000`; `SQLITE_TOOBIG`, `SQLITE_MISMATCH`) is dead-lettered as
a constraint violation is, and only an error about the store itself (class
`42`, `3F`, `54001` and up; `SQLITE_ERROR`, `CORRUPT`, `NOTADB`, `RANGE`)
degrades. The adapters map a value-scoped error to `StoreError::Constraint`
(`Cause.errSplit`), so the loop is `runClassed` unchanged: only the
classification moves. `Cause.cls` forgets the scope, which gives back the
classes above, so every theorem about `Cls` still holds of the projection.

* `value_rejected_dead_letters`: one call, the batch dead-lettered and
  counted once, and the session goes on (not degraded, not fenced, running,
  no hold).
* `store_permanent_degrades_split`: a store-scoped error degrades as before.
* `via_split_agrees`: on a store with no value-scoped error, the split is the
  fix above.
* `no_silent_loss_split`, `close_ok_accounted_split`: the invariants and the
  close theorem hold of the split classification (they hold of `runClassed`
  over any store). -/

/-- The class of a failed flush with the scope of a permanent error. -/
inductive Cause where
  | retryable
  | constraint
  | stale
  /-- A value the store cannot hold: the batch's fault. -/
  | valueRejected
  /-- The store itself: a missing table, a corrupt file. -/
  | storePermanent
  deriving DecidableEq, Repr

/-- Forget the scope. -/
def Cause.cls : Cause → Cls
  | .retryable => .retryable
  | .constraint => .constraint
  | .stale => .stale
  | .valueRejected => .permanent
  | .storePermanent => .permanent

/-- The remediated mapping: a value-scoped error reaches the loop as
`StoreError::Constraint`, a store-scoped one as `StoreError::Permanent`. -/
def Cause.errSplit : Cause → Err
  | .valueRejected => .constraint
  | c => c.cls.errFixed

inductive KRes where
  | ok
  | fail (c : Cause)
  | lostAck
  deriving DecidableEq, Repr

def KRes.outcome (m : Cause → Err) : KRes → Outcome
  | .ok => .ok
  | .fail c => .fail (m c)
  | .lostAck => .lostAck

def KRes.res : KRes → Res
  | .ok => .ok
  | .fail c => .fail c.cls
  | .lostAck => .lostAck

/-- A store described by class and scope. -/
abbrev KStore (α : Type) := List α → Nat → KRes

def KStore.via (m : Cause → Err) (ks : KStore α) : Store α := fun b n => (ks b n).outcome m

/-- The same store with the scope forgotten. -/
def KStore.cls (ks : KStore α) : CStore α := fun b n => (ks b n).res

/-- Any mapping that ignores the scope sees the projected store. -/
theorem via_cls (m : Cls → Err) (ks : KStore α) :
    CStore.via m ks.cls = KStore.via (fun c => m c.cls) ks := by
  funext b n
  simp only [CStore.via, KStore.via, KStore.cls]
  cases ks b n <;> rfl

/-- A value-scoped error is not retried. -/
theorem value_rejected_not_retryable : (Cause.errSplit .valueRejected).retryable = false := rfl

/-- Nor is a store-scoped one, and a transient stays retryable. -/
theorem store_permanent_not_retryable : (Cause.errSplit .storePermanent).retryable = false := rfl

theorem transient_retryable_split : (Cause.errSplit .retryable).retryable = true := rfl

/-- **Agreement.** On a store that never rejects a value, the split is the
fix before it. -/
theorem via_split_agrees {ks : KStore α} (h : ∀ b n, ks b n ≠ .fail .valueRejected) :
    KStore.via Cause.errSplit ks = CStore.via Cls.errFixed ks.cls := by
  rw [via_cls]
  funext b n
  have hb := h b n
  simp only [KStore.via]
  cases hc : ks b n with
  | ok => rfl
  | lostAck => rfl
  | fail c =>
    rw [hc] at hb
    cases c <;> simp_all [KRes.outcome, Cause.errSplit]

section Split

variable {P : Params}

/-- **A rejected value is a dead letter, and the session goes on.** One
call, no retry; the whole batch goes to the ledger as a dead letter and
`dead_lettered` rises by one; nothing is held, degraded or fenced, so the
next batch is sent on the next due tick (contrast `exBadValue` under the
fix before, which degrades). -/
theorem value_rejected_dead_letters {ks : KStore α} {s : St α} {now : Nat} {er : Bool}
    (hrun : s.running = true) (hf : s.fenced = false) (h : Due P true now s)
    (hv : ks (s.pending ++ s.log) s.attempts = .fail .valueRejected) :
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).calls =
      s.calls ++ [(s.pending ++ s.log, .fail .constraint)] ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).ledger =
      s.ledger ++ [(.deadLettered, s.pending ++ s.log)] ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).deadLettered =
      s.deadLettered + 1 ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).pending = [] ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).retryAfter = none ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).degraded = false ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).fenced = false ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now er s).running = true := by
  have hs : KStore.via Cause.errSplit ks (s.pending ++ s.log) s.attempts = .fail .constraint := by
    simp [KStore.via, hv, KRes.outcome, Cause.errSplit]
  have hrep : (KStore.via Cause.errSplit ks (s.pending ++ s.log) s.attempts).reported =
      some .constraint := by rw [hs]; rfl
  have hres : flushWithRetry (KStore.via Cause.errSplit ks) (s.pending ++ s.log) s.attempts
      P.retries = ⟨[(s.pending ++ s.log, .fail .constraint)], [], some .constraint⟩ := by
    unfold flushWithRetry
    rw [ladder_nonretryable hrep rfl, hs]
  have hr' : (!s.running) = false := by simp [hrun]
  simp only [iterClassed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleClassed_with, cycleWith_due _ hf h]
  simp [settleClassed, settleFixed, preSettle, checkFence, hf, hrun, hres, h.notDegraded]

/-- **A store-scoped error degrades**, as under the fix before. -/
theorem store_permanent_degrades_split {ks : KStore α} {s : St α} {now : Nat}
    (hrun : s.running = true) (hf : s.fenced = false) (h : Due P true now s)
    (hp : ks (s.pending ++ s.log) s.attempts = .fail .storePermanent) :
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).calls =
      s.calls ++ [(s.pending ++ s.log, .fail .permanent)] ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).pending =
      s.pending ++ s.log ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).ledger = s.ledger ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).deadLettered =
      s.deadLettered ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).degraded = true ∧
    (iterClassed P (KStore.via Cause.errSplit ks) .tick now false s).running = true := by
  have hs : KStore.via Cause.errSplit ks (s.pending ++ s.log) s.attempts = .fail .permanent := by
    simp [KStore.via, hp, KRes.outcome, Cause.errSplit, Cause.cls, Cls.errFixed]
  have hres := permanent_one_call (st := KStore.via Cause.errSplit ks) hs P.retries
  have hr' : (!s.running) = false := by simp [hrun]
  simp only [iterClassed, hr', Bool.false_eq_true, ↓reduceIte]
  rw [cycleClassed_with, cycleWith_due _ hf h]
  simp [settleClassed, preSettle, checkFence, hf, hrun, hres]

/-- `no_silent_loss_classed` under the split classification. -/
theorem no_silent_loss_split (ks : KStore α) (evs : List (PEv α)) :
    ∀ e ∈ (runClassedGuarded P (KStore.via Cause.errSplit ks) St.init evs).ledger,
      e.1 = .committed ∨
      (e.1 = .deadLettered ∧
        0 < (runClassedGuarded P (KStore.via Cause.errSplit ks) St.init evs).deadLettered) ∨
      (e.1 = .droppedDegraded ∧
        (runClassedGuarded P (KStore.via Cause.errSplit ks) St.init evs).degraded = true) ∨
      (e.1 = .droppedFenced ∧
        (runClassedGuarded P (KStore.via Cause.errSplit ks) St.init evs).fenced = true) :=
  no_silent_loss_classed evs

/-- `close_ok_accounted_classed` under the split classification: a dead
letter is still the one drop an `Ok` close can sit over, and it is counted. -/
theorem close_ok_accounted_split {ks : KStore α} {s : St α} (hg : Good s)
    (evs : List (PEv α)) (final : Store α)
    (hok : closeOkClassedGuarded P (KStore.via Cause.errSplit ks) true s evs final = true)
    (hstop : (runClassedGuarded P (KStore.via Cause.errSplit ks) s evs).running = false) :
    (∀ e ∈ (runClassedGuarded P (KStore.via Cause.errSplit ks) s evs).ledger,
      e.1 = .committed ∨ e.1 = .deadLettered) ∧
    (runClassedGuarded P (KStore.via Cause.errSplit ks) s evs).consumed ++
        (runClassedGuarded P (KStore.via Cause.errSplit ks) s evs).log =
      (runClassedGuarded P (KStore.via Cause.errSplit ks) s evs).hist :=
  close_ok_accounted_classed hg evs final hok hstop

end Split

end Classes

/-! ## Examples -/

section Examples

/-- `retries = 2`, `max_batch = 10`, `log_max = 100`. -/
def exP : Params := { retries := 2, maxBatch := 10, logMax := 100 }

/-- First attempt fails with a backend error, the retry succeeds. -/
def exFlaky : Store Nat := fun _ n => if n = 0 then .fail .backend else .ok

/-- `transient_retried`: the batch is sent twice, whole, and committed once. -/
example :
    (run exP exFlaky St.init [.write 1, .write 2, .wake .tick 0 false]).calls =
      [([1, 2], .fail .backend), ([1, 2], .ok)] ∧
    (run exP exFlaky St.init [.write 1, .write 2, .wake .tick 0 false]).ledger =
      [(.committed, [1, 2])] := by
  decide

/-- A store that never answers. -/
def exDown : Store Nat := fun _ _ => .fail .backend

/-- `exhausted_retained`, `flushWithRetry_backoff`: three attempts, sleeps
100 ms and 200 ms, the batch retained and held for `RETAINED_BACKOFF` past the
ladder. -/
example :
    (run exP exDown St.init [.write 1, .wake .tick 0 false]).calls.length = 3 ∧
    (run exP exDown St.init [.write 1, .wake .tick 0 false]).pending = [1] ∧
    (run exP exDown St.init [.write 1, .wake .tick 0 false]).retryAfter = some 10300 := by
  decide

/-- `flushWithRetry_backoff`: with nine retries the sleeps double to the cap
and stay there. -/
example :
    (flushWithRetry exDown [1] 0 9).sleeps =
      [100, 200, 400, 800, 1600, 3200, 6400, 10000, 10000] := by
  decide

/-- `cycle_held`: during the post-retry hold a tick makes no call; after it, the
retained batch is sent again, with the newer write behind it. -/
example :
    (run exP exDown St.init [.write 1, .wake .tick 0 false, .write 2, .wake .tick 5000 false]).calls.length
      = 3 ∧
    (run exP exDown St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 5000 false, .wake .tick 10300 false]).calls.getLast?
      = some ([1, 2], .fail .backend) := by
  decide

/-- `cycle_poll_waits`: a poll below `max_batch` makes no call. -/
example : (run exP exDown St.init [.write 1, .wake .poll 0 false]).calls = [] := by decide

/-- Down for the first three attempts; any batch holding `3` violates a
constraint. -/
def exPoison : Store Nat := fun b n =>
  if b.contains 3 then .fail .constraint else if n < 3 then .fail .backend else .ok

/-- `constraint_dead_lettered`, `never_resent`: the dead letter is one call,
counted once, never sent again. It takes the retained `[1, 2]` with it, though
`[1, 2]` alone would now commit: the batch dead-lettered is the whole merged
`pending`, not the write that violated the constraint. -/
example :
    (run exP exPoison St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .tick 20000 false,
       .write 4, .wake .tick 40000 false]).ledger =
      [(.deadLettered, [1, 2, 3]), (.committed, [4])] ∧
    (run exP exPoison St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .tick 20000 false,
       .write 4, .wake .tick 40000 false]).deadLettered = 1 ∧
    (run exP exPoison St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .tick 20000 false,
       .write 4, .wake .tick 40000 false]).calls.drop 3 =
      [([1, 2, 3], .fail .constraint), ([4], .ok)] ∧
    exPoison [1, 2] 4 = .ok := by
  decide

/-- The holder lost the lease: every write is refused as stale. -/
def exStale : Store Nat := fun _ _ => .fail .staleWrite

/-- A realizable run with a 5 s interval: ticks at 0, 5, 10, 15 and 20 s.
The polls every 100 ms in between are left out except the last one before the
fence: each drains nothing new, makes no call (below `max_batch`) and finds
the fence unlatched, so it changes nothing. The fence latches at about
19.95 s, within 100 ms before the 20 s tick. -/
def exStaleRun : List (Ev Nat) :=
  [.write 1, .wake .tick 0 false, .write 2, .wake .tick 5000 false,
   .wake .tick 10000 false, .wake .tick 15000 false, .wake .poll 19900 false, .fence,
   .wake .tick 20000 false]

/-- `stale_write_one_call`, `stale_write_retained`, `retained_resent`,
`cycle_held`, `flush_after_fence_latched`: each attempt is a single call, but
the batch is retained and sent again once the hold has passed (not at 5 s or
15 s); the fence latched just before the 20 s tick is seen only after that
tick has sent the batch a third time. The tail is then dropped, not
dead-lettered. -/
example :
    (run exP exStale St.init exStaleRun).calls =
      [([1], .fail .staleWrite), ([1, 2], .fail .staleWrite), ([1, 2], .fail .staleWrite)] ∧
    (run exP exStale St.init exStaleRun).ledger = [(.droppedFenced, [1, 2])] ∧
    (run exP exStale St.init exStaleRun).deadLettered = 0 := by
  decide

/-- `stop_requeues`: a stop that ends the wait after the fence latched hands
the tail back to the graph log instead of dropping it. -/
example :
    (run exP exStale St.init [.write 1, .wake .tick 0 false, .fence, .wake .stop 1 false]).log
      = [1] ∧
    (run exP exStale St.init [.write 1, .wake .tick 0 false, .fence, .wake .stop 1 false]).ledger
      = [] := by
  decide

/-- The Rust test `a_fenced_loop_stops_flushing_and_drops_pending`: three
mutations, the fence latched while the loop waits, then a poll: the poll's
cycle drains, makes no call (3 < `max_batch` = 100), and the fence check drops
the tail. -/
example :
    (run { exP with maxBatch := 100 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).calls = [] ∧
    (run { exP with maxBatch := 100 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).ledger =
      [(.droppedFenced, [1, 2, 3])] := by
  decide

/-- `flush_after_fence_latched`: the same test with `max_batch = 3` (or the
interval tick instead of the poll) sends the tail to the store after the
fence latched, before the check drops it. -/
example :
    (run { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).calls =
      [([1, 2, 3], .fail .backend), ([1, 2, 3], .ok)] ∧
    (run { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).ledger =
      [(.committed, [1, 2, 3])] ∧
    (run { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).running = false := by
  decide

/-- `stale_write_erased`: an erased session fences at once and drops on the
next wake-up. -/
example :
    (run exP exStale St.init [.write 1, .wake .tick 0 true, .wake .poll 1 false]).ledger =
      [(.droppedFenced, [1])] ∧
    (run exP exStale St.init [.write 1, .wake .tick 0 true, .wake .poll 1 false]).calls.length = 1 := by
  decide

/-- `stop_requeues`, `stopped_accounted`: a retained batch goes back to the
front of the graph log on stop, ahead of the newer write. -/
example :
    (run exP exDown St.init [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .stop 0 false]).log
      = [1, 2, 3] ∧
    (run exP exDown St.init [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .stop 0 false]).pending
      = [] ∧
    (run exP exDown St.init [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .stop 0 false]).ledger
      = [] := by
  decide

/-- `degraded_silent`: past `log_max` the session degrades, then drops what
it drains and makes no further call. -/
example :
    (run { exP with logMax := 1 } exDown St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .tick 1 false]).ledger =
      [(.droppedDegraded, [1, 2, 3])] ∧
    (run { exP with logMax := 1 } exDown St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .tick 1 false]).calls = [] := by
  decide

/-- `abort_loses`: a hard abort of a loop holding `[1]` leaves it nowhere. -/
example :
    let s := abort (run exP exDown St.init [.write 1, .wake .tick 0 false])
    s.consumed ++ s.pending ++ s.log = [] ∧ s.hist = [1] := by
  decide

/-- The lease of `fencing_refuses_after_takeover`'s example, built with the
`Lease` operations: holder `0` acquires at time 0 (token 1, TTL 10); holder
`1` takes the expired row over at time 20 (token 2), before attempt `2`. The
released marker is `99`. -/
def exA : Lease.Row Nat := Lease.first 0 0 10

def exRowT (n : Nat) : Lease.Row Nat := if n < 2 then exA else Lease.acquire 1 20 10 exA

/-- `fencing_refuses_after_takeover` applied: its `Lease.Steps` and `hother`
hypotheses hold for `exRowT`, so from attempt `2` every attempt with token `1`
is refused. -/
example : ∀ b n, 2 ≤ n → fencing exRowT (some exA.token) exFlaky b n = .fail .staleWrite :=
  fencing_refuses_after_takeover (released := 99) (A := 0) rfl exRowT exFlaky 2
    (fun n hn => by
      have e : exRowT n = Lease.acquire 1 20 10 exA := by
        simp [exRowT, Nat.not_lt.mpr hn]
      rw [e]
      exact Lease.Steps.tail (Lease.Steps.refl _) (Lease.Step.acq 1 20 10 exA (by decide)))
    (fun n hn => by
      have e : exRowT n = Lease.acquire 1 20 10 exA := by
        simp [exRowT, Nat.not_lt.mpr hn]
      rw [e]
      decide)

/-- The same rows, concretely: the takeover mints token 2 for holder 1, and a
ladder before the takeover runs normally while one after it is one refused
call. -/
example :
    exA.token = 1 ∧ (exRowT 2).holder = 1 ∧ (exRowT 2).token = 2 ∧
    (flushWithRetry (fencing exRowT (some 1) exFlaky) [7] 0 2).calls =
      [([7], .fail .backend), ([7], .ok)] ∧
    (flushWithRetry (fencing exRowT (some 1) exFlaky) [7] 2 2).calls =
      [([7], .fail .staleWrite)] := by
  decide

/-- `override_refresh_retakes`: holder `0` holds token 1 (TTL 45 s); the
operator override at 50 s releases the row; holder `0`'s own refresh at 60 s
re-takes it at token 2. -/
def exRetakenRow : Lease.Row Nat :=
  Lease.acquire 0 60000 45000 (Lease.release 99 0 50000 (Lease.first 0 0 45000))

/-- The store as the wedged holder's flush loop sees it from then on. -/
def exRetaken : Store Nat := fencing (fun _ => exRetakenRow) (some 1) exFlaky

/-- The refresh comes back `Acquired` (holder `0` again), so the heartbeat
does not latch; every flush is refused, retained and resent every 10 s, and
the handle keeps accepting writes. -/
example :
    exRetakenRow.holder = 0 ∧ exRetakenRow.token = 2 ∧
    (run exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).calls.length = 4 ∧
    (run exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).pending = [1, 2, 3] ∧
    (run exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).fenced = false := by
  decide

/-- ...until the backlog passes `log_max` and the session degrades. -/
example :
    (run { exP with logMax := 2 } exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .write 3, .wake .tick 20000 false]).degraded = true ∧
    (run { exP with logMax := 2 } exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .write 3, .wake .tick 20000 false]).calls.length = 2 := by
  decide

/-! ### The same scenarios in the fixed loop (#293) -/

/-- The issue's scenario, fixed (`stale_write_never_resent`): the wedged
holder's first flush after the re-take is refused once, the stale write
latches the fence, the batch is dropped as a fenced drop and the loop stops.
No resend, and nothing waits for `log_max` to degrade the session. (The fixed
heartbeat latches too, `Lease.override_beat_latches`; this run does not need
it.) -/
example :
    (runFixed exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).calls =
      [([1], .fail .staleWrite)] ∧
    (runFixed exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).ledger =
      [(.droppedFenced, [1])] ∧
    (runFixed exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).fenced = true ∧
    (runFixed exP exRetaken St.init
      [.write 1, .wake .tick 0 false, .write 2, .wake .tick 10000 false,
       .wake .tick 20000 false, .write 3, .wake .tick 30000 false]).running = false := by
  decide

/-- `exStaleRun`, fixed: the first refused flush fences the loop; the batch is
never sent again. -/
example :
    (runFixed exP exStale St.init exStaleRun).calls = [([1], .fail .staleWrite)] ∧
    (runFixed exP exStale St.init exStaleRun).ledger = [(.droppedFenced, [1])] ∧
    (runFixed exP exStale St.init exStaleRun).deadLettered = 0 := by
  decide

/-- `fence_before_due_tick_fixed`: the `max_batch = 3` run that flushed after
the fence latched now makes no call and drops the tail. -/
example :
    (runFixed { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).calls = [] ∧
    (runFixed { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).ledger =
      [(.droppedFenced, [1, 2, 3])] ∧
    (runFixed { exP with maxBatch := 3 } exFlaky St.init
      [.write 1, .write 2, .write 3, .fence, .wake .poll 100 false]).running = false := by
  decide

/-- `stop_drops_fenced`: a retained batch, then the fence, then a stop. The
old loop hands `[1]` back to the graph log; the fixed loop drops it. -/
example :
    (run exP exDown St.init [.write 1, .wake .tick 0 false, .fence, .wake .stop 1 false]).log
      = [1] ∧
    (runFixed exP exDown St.init [.write 1, .wake .tick 0 false, .fence, .wake .stop 1 false]).log
      = [] ∧
    (runFixed exP exDown St.init
      [.write 1, .wake .tick 0 false, .fence, .wake .stop 1 false]).ledger =
      [(.droppedFenced, [1])] := by
  decide

/-- `stepFixed_agrees`: away from the fence and stale writes the fixed loop
runs exactly as before (the retry, the retained batch, the stop). -/
example :
    runFixed exP exFlaky St.init [.write 1, .write 2, .wake .tick 0 false] =
      run exP exFlaky St.init [.write 1, .write 2, .wake .tick 0 false] ∧
    runFixed exP exDown St.init
      [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .stop 0 false] =
      run exP exDown St.init
        [.write 1, .write 2, .wake .tick 0 false, .write 3, .wake .stop 0 false] := by
  decide

/-! ### `Memory::close` over the fixed loop -/

/-- **The close that read the fence only before the stop is not honest** over
the fixed loop: a batch in flight when close starts is refused as stale, the
loop drops it, and the close finds an empty log and reports `Ok`. The
re-checking close reports the error. -/
theorem close_ok_over_drop :
    closeOk exP exStale false (run exP exStale St.init [.write 1])
      [.wake .tick 0 false, .wake .stop 1 false] exStale = true ∧
    (runFixed exP exStale (run exP exStale St.init [.write 1])
      [.wake .tick 0 false, .wake .stop 1 false]).ledger = [(.droppedFenced, [1])] ∧
    closeOk exP exStale true (run exP exStale St.init [.write 1])
      [.wake .tick 0 false, .wake .stop 1 false] exStale = false := by
  decide

/-- Before #293 the same run was honest without the re-check: the old loop
retained the refused batch and the stop handed it back, so close's final
flush saw `[1]` and was refused. -/
example :
    (run exP exStale (run exP exStale St.init [.write 1])
      [.wake .tick 0 false, .wake .stop 1 false]).log = [1] := by
  decide

/-! ### The two unguarded store calls (#301) -/

/-- One write, not yet flushed. -/
def exOne : St Nat := run exP exDown St.init [.write 1]

/-- The store is down; the tick's ladder fails, `erased()` reads the lease and
the read panics; then `close` stops the loop. -/
def exLeasePanic : List (PEv Nat) := [.wake .tick 0 .panic none, .wake .stop 1 (.ret false) none]

/-- The same, the read answering "not erased" and the stats write panicking. -/
def exStatsPanic : List (PEv Nat) :=
  [.wake .tick 0 (.ret false) (some .panic), .wake .stop 1 (.ret false) none]

/-- The same, the read never returning. -/
def exLeaseHang : List (PEv Nat) := [.wake .tick 0 .hang none, .wake .stop 1 (.ret false) none]

/-- **The issue's scenario, before #301** (`lease_panic_loses`,
`stats_panic_loses`): after three refused calls the task dies holding `[1]`.
`close` logs the join error, finds an empty log and an undegraded session, and
returns `Ok`, with a final store that would have taken the batch. `1` was never
committed and is nowhere: not in the ledger, `pending` or the log. -/
theorem close_ok_over_panic :
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.calls.length = 3 ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.ledger = [] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.pending = [] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.log = [] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.hist = [1] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeasePanic).st.degraded = false ∧
    closeOkUnguarded exP exDown true ⟨exOne, false⟩ exLeasePanic exFlaky = some true ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exStatsPanic).st.ledger = [] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exStatsPanic).st.log = [] ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exStatsPanic).st.pending = [] ∧
    closeOkUnguarded exP exDown true ⟨exOne, false⟩ exStatsPanic exFlaky = some true := by
  decide

/-- `lease_hang_blocks_stop`: the read never returns, the stop is never served,
`close` never returns (`none`), and `[1]` stays in the parked task. -/
example :
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeaseHang).hung = true ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeaseHang).st.running = true ∧
    (runUnguarded exP exDown ⟨exOne, false⟩ exLeaseHang).st.pending = [1] ∧
    closeOkUnguarded exP exDown true ⟨exOne, false⟩ exLeaseHang exFlaky = none := by
  decide

/-- **The same scenarios after #301** (`no_silent_loss_guarded`,
`stop_exits_guarded`, `close_ok_accounted_guarded`): a panicking or hanging
read counts as "not erased" and a panicking stats write is ignored, so in all
three the batch is retained, the stop hands `[1]` back to the graph log, and
`close`'s final flush decides: `Ok` when it commits `[1]`, an error when the
store still refuses it. -/
example :
    (runGuarded exP exDown exOne exLeasePanic).log = [1] ∧
    (runGuarded exP exDown exOne exLeasePanic).running = false ∧
    (runGuarded exP exDown exOne exLeasePanic).ledger = [] ∧
    runGuarded exP exDown exOne exStatsPanic = runGuarded exP exDown exOne exLeasePanic ∧
    runGuarded exP exDown exOne exLeaseHang = runGuarded exP exDown exOne exLeasePanic ∧
    closeOkGuarded exP exDown true exOne exLeasePanic exFlaky = true ∧
    closeOkGuarded exP exDown true exOne exLeasePanic exDown = false ∧
    closeOkGuarded exP exDown true exOne exLeaseHang exDown = false := by
  decide

/-- `runGuarded_lift`, `closeOkUnguarded_agrees`: where the calls return, the
fix changes nothing (here the read answers "not erased"). -/
example :
    runUnguarded exP exDown ⟨exOne, false⟩
        [.wake .tick 0 (.ret false) (some (.ret ())), .wake .stop 1 (.ret false) none] =
      ⟨runGuarded exP exDown exOne exLeasePanic, false⟩ := by
  decide

/-! ### Error classes (#301) -/

/-- The store refuses every batch with a permanent error that is a property of
the store, not of the batch: SQLSTATE `42P01`, the table is gone. -/
def exGone : CStore Nat := fun _ _ => .fail .permanent

/-- Two writes, a tick after each, then `close` stops the loop. -/
def exGoneEvs : List (Ev Nat) :=
  [.write 1, .wake .tick 0 false, .write 2, .wake .tick 20000 false, .wake .stop 20001 false]

/-- **Before the fix** (`permanent_retried_retained`): three calls, each seen
as `Backend`, then `[1]` is retained and held, and the session is not
degraded; after the hold `[1, 2]` gets three more. -/
example :
    (runFixed exP (CStore.via Cls.errOld exGone) St.init [.write 1, .wake .tick 0 false]).calls =
      [([1], .fail .backend), ([1], .fail .backend), ([1], .fail .backend)] ∧
    (runFixed exP (CStore.via Cls.errOld exGone) St.init [.write 1, .wake .tick 0 false]).pending
      = [1] ∧
    (runFixed exP (CStore.via Cls.errOld exGone) St.init
      [.write 1, .wake .tick 0 false]).degraded = false ∧
    (runFixed exP (CStore.via Cls.errOld exGone) St.init
      (exGoneEvs.take 4)).calls.length = 6 := by
  decide

/-- And it degrades in the end, once the retained backlog passes `log_max`
(here 2). -/
example :
    (runFixed { exP with logMax := 2 } (CStore.via Cls.errOld exGone) St.init
      [.write 1, .wake .tick 0 false, .write 2, .write 3, .wake .tick 20000 false]).degraded =
      true := by
  decide

/-- **After the fix** (`permanent_degrades_classed`,
`permanent_never_resent_classed`): one call, the session degrades holding
`[1]`, the next tick drops `[1, 2]` as degraded with no call, and `close`
returns an error. -/
example :
    (runClassed exP (CStore.via Cls.errFixed exGone) St.init exGoneEvs).calls =
      [([1], .fail .permanent)] ∧
    (runClassed exP (CStore.via Cls.errFixed exGone) St.init exGoneEvs).ledger =
      [(.droppedDegraded, [1, 2])] ∧
    (runClassed exP (CStore.via Cls.errFixed exGone) St.init exGoneEvs).degraded = true ∧
    closeOkClassed exP (CStore.via Cls.errFixed exGone) true St.init exGoneEvs
      (CStore.via Cls.errFixed exGone) = false := by
  decide

/-- **The candidate not taken.** Dead-lettering a permanent error: each batch
is one call, dead-lettered and counted, the session goes on, and `close`
returns `Ok` with nothing committed. Every acknowledged write is lost, and
only `dead_lettered` says so. -/
example :
    (runFixed exP (CStore.via Cls.errDeadLetter exGone) St.init exGoneEvs).ledger =
      [(.deadLettered, [1]), (.deadLettered, [2])] ∧
    (runFixed exP (CStore.via Cls.errDeadLetter exGone) St.init exGoneEvs).committed = [] ∧
    closeOk exP (CStore.via Cls.errDeadLetter exGone) true St.init exGoneEvs
      (CStore.via Cls.errDeadLetter exGone) = true := by
  decide

/-- A permanent error that is a property of the batch: the store rejects any
batch holding the value `3` (a value it cannot store, e.g. SQLSTATE `22021`).
A value-scoped error (`Cause.valueRejected`); `exBadValue.cls` is the same
store with the scope forgotten, as the fix before the split saw it. -/
def exBadValue : KStore Nat := fun b _ => if b.contains 3 then .fail .valueRejected else .ok

def exBadEvs : List (Ev Nat) :=
  [.write 1, .wake .tick 0 false, .write 3, .wake .tick 1000 false, .write 4,
   .wake .tick 2000 false]

/-- **A rejected value is a dead letter** (`value_rejected_dead_letters`).
Under the split classification `[1]` commits, `[3]` is dead-lettered and
counted, and `[4]` commits: the session stays durable. The fix before the
split degraded on `[3]` and dropped `[4]` with it, which was the cost of
degrading; the code before #301 resends `[3]` until the backlog passes
`log_max`, then degrades all the same; here it still holds `[3, 4]`. -/
example :
    (runClassed exP (KStore.via Cause.errSplit exBadValue) St.init exBadEvs).ledger =
      [(.committed, [1]), (.deadLettered, [3]), (.committed, [4])] ∧
    (runClassed exP (KStore.via Cause.errSplit exBadValue) St.init exBadEvs).deadLettered = 1 ∧
    (runClassed exP (KStore.via Cause.errSplit exBadValue) St.init exBadEvs).degraded = false ∧
    (runClassed exP (CStore.via Cls.errFixed exBadValue.cls) St.init exBadEvs).ledger =
      [(.committed, [1]), (.droppedDegraded, [3, 4])] ∧
    (runFixed exP (CStore.via Cls.errDeadLetter exBadValue.cls) St.init exBadEvs).ledger =
      [(.committed, [1]), (.deadLettered, [3]), (.committed, [4])] ∧
    (runFixed exP (CStore.via Cls.errOld exBadValue.cls) St.init exBadEvs).pending = [3, 4] ∧
    (runFixed exP (CStore.via Cls.errOld exBadValue.cls) St.init exBadEvs).degraded =
      false := by
  decide

/-- `exGone` as a store-scoped error: the split still degrades on it
(`store_permanent_degrades_split`, `via_split_agrees`), and `close` errors. -/
def exGoneK : KStore Nat := fun _ _ => .fail .storePermanent

example :
    runClassed exP (KStore.via Cause.errSplit exGoneK) St.init exGoneEvs =
      runClassed exP (CStore.via Cls.errFixed exGone) St.init exGoneEvs ∧
    closeOkClassed exP (KStore.via Cause.errSplit exGoneK) true St.init exGoneEvs
      (KStore.via Cause.errSplit exGoneK) = false := by
  decide

/-- `via_fixed_agrees`, `runClassed_old`: a transient outage is retried and
committed exactly as before the fix. -/
example :
    runClassed exP (CStore.via Cls.errFixed (fun _ n => if n = 0 then .fail .retryable else .ok))
        St.init [.write 1, .write 2, .wake .tick 0 false] =
      run exP exFlaky St.init [.write 1, .write 2, .wake .tick 0 false] := by
  decide

end Examples

end Lambo.Flush
