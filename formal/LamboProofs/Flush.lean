import LamboProofs.Lease

/-
# The store flush path: retry ladder, dead letters, fence and shutdown

Model of the write-behind flush loop in `src/store/flush.rs`: `FlushLoop::run`
(the top-of-loop fence check and the `biased` `select!`), `cycle` /
`drain_and_flush`, `flush_with_retry`, `requeue_pending`, `drop_pending`, the
constants `BACKOFF_BASE` (100 ms), `BACKOFF_CAP` (10 s) and `RETAINED_BACKOFF`
(= the cap), `FlushParams` (`retries`, `max_batch`, `log_max`) and
`FlushStats::dead_lettered`; `StoreError::is_retryable` in `src/types/mod.rs`;
and two clauses of the `GraphStore::flush` contract in `src/store/mod.rs`, the
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
  one to `dead_lettered` ("never retried, never retained").
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

## What the model shows about the Rust

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

## Left out

Read-access drains and their budget (#30); the `mutation_epoch` and GC-mark
carry (#17, #29); the graph's unflushed set (`mark_durable_through`,
`pin_unflushed`, #60); `lag`, `depth` and `publish_stats`; writes that land
during a cycle (they land between iterations here); the `select!` priority
(the event list picks the branch); `refused_as_erased`'s store read (an input,
`erased`); `Memory::close`'s own final flush. The model also drops an
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
a panicking backend (`CatchUnwindPoll`) both reach the ladder as `Backend`. -/
inductive Err where
  | sessionNotFound
  | notFound
  | capability
  | invariant
  | backend
  | constraint
  | staleWrite
  | other
  deriving DecidableEq, Repr

/-- `StoreError::is_retryable`: everything except `Constraint` and
`StaleWrite`. -/
def Err.retryable : Err → Bool
  | .constraint => false
  | .staleWrite => false
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

def backoffBase : Nat := 100
def backoffCap : Nat := 10000
def retainedBackoff : Nat := backoffCap

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

end Examples

end Lambo.Flush
