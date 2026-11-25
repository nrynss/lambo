import LamboProofs.Flush.Retry

/-
# The write-behind flush loop (`src/store/flush.rs`)

Models `FlushLoop::run` (the top-of-loop fence check and the `biased`
`select!`), `cycle` / `drain_and_flush` with its early returns in the Rust
order, `requeue_pending`, `drop_pending`, `FlushParams` and
`FlushStats::dead_lettered`, over `Retry.lean`'s ladder: the loop before #293
(`cycle`, `iter`, `step`, `run`), after #293 (`cycleFixed`, `runFixed`, with
`Lease.beatLatches` the fixed heartbeat), and after #301 item 2 (the two
store calls `refused_as_erased` and `publish_stats` unguarded, `runUnguarded`,
and guarded, `runGuarded`). Ghost state records every mutation appended
(`hist`), every batch that left `pending` with its fate (`ledger`) and every
store call (`calls`). Those results cover the ordinary flush path. The
replay isolation model at the end describes the separate attempts triggered
when the store rejects a mixed batch (#324).

Results: over every run nothing is lost or duplicated while the loop runs
(`Good.conserve`); every batch sent is the whole unconsumed tail, oldest
first (`step_calls_unconsumed`, `committed_sublist`), and nothing that left
`pending` is sent again (`never_resent`); a retryable failure is retried and
then retained (`exhausted_retained`), a constraint violation dead-lettered
with one call and counted (`constraint_dead_lettered`); after a stop every
mutation was committed, dead-lettered, dropped by a degraded or fenced
session, or handed back for `close` (`no_silent_loss`). Before #293 a
`StaleWrite` was retained and resent (`stale_write_retained`), a refresh could
re-take the lease so the fence never latched (`override_refresh_retakes`), and
a fence latched during the wait let one more flush through
(`flush_after_fence_latched`); since #293 the heartbeat latches after any
takeover and the loop never calls the store again (`override_fences_fixed`,
`no_flush_after_fence_fixed`), a stale write is never resent
(`stale_write_never_resent`), and the invariants still hold
(`no_silent_loss_fixed`); a close that re-reads the fence after the join
returns `Ok` only when nothing was dropped (`close_ok_no_drop`). Before #301 a
panic in either unguarded call lost the batch (`lease_panic_loses`) and a hang
blocked the stop (`lease_hang_blocks_stop`); guarded, the #293 results carry
over (`no_silent_loss_guarded`, `stop_exits_guarded`).

Boundary: the event list picks the `select!` branch and wake-ups may be spaced
arbitrarily (the Rust wakes at least every `POLL_QUANTUM`), so some runs are
not realizable; the theorems hold of every run. The erased answer is an input
(that a wrong "not erased" never writes rests on the adapters' commit-time
fence, `erase::check_fence`, not modelled). Not modelled: read-access drains
(#30), the `mutation_epoch` and GC-mark carry, the unflushed set (#60), what
`publish_stats` writes, writes landing during a cycle, and `Memory::close`'s
own final flush beyond the `closeOk` outcome.
-/

namespace Lambo.Flush

open Lambo.Lib (prefix_drop)

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

/-- While the fence is not latched, `drain_and_flush`'s fence check leaves the
loop's state unchanged. -/
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

/-- Once the flush loop has stopped, no event makes another store call or adds
to the ledger. -/
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

/-- A degraded flush loop makes no further store call: `drain_and_flush` keeps
the session's writes in memory. -/
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

/-! ## Isolating a rejected replay batch (#324)

The first store call has already rejected the whole mixed batch. Each probe
then offers a chronological span. Replay effects and their consume make one
span. A deterministic rejection of a replay span permits a separate attempt
to persist its failure consume. The result below records only confirmed store
commits. A transient, fenced or unacknowledged attempt ends this pass and
has no durable effect in the model. -/

inductive ReplayProbe (span : Type)
  | committed (batch : span)
  | settledRejection (failedConsume : span)
  | stopped

/-- The prefix confirmed durable by the isolation pass. After a rejection,
later replay writes may depend on rejected effects still present in RAM.
Their spans are deferred to the next serve. -/
def replayRecoveryLog : List (ReplayProbe span) → List span
  | [] => []
  | .committed batch :: rest => batch :: replayRecoveryLog rest
  | .settledRejection failedConsume :: _ => [failedConsume]
  | .stopped :: _ => []

/-- Confirmed prefixes stay durable, the rejected span writes only its failed
consume, and a suffix produced from speculative state stays uncommitted. -/
theorem replay_recovery_stops_at_rejection (confirmed : List span) (failure : span)
    (suffix : List (ReplayProbe span)) :
    replayRecoveryLog (confirmed.map ReplayProbe.committed ++
      .settledRejection failure :: suffix) = confirmed ++ [failure] := by
  induction confirmed with
  | nil => rfl
  | cons batch rest ih => simp [replayRecoveryLog, ih]

/-- Failure of the standalone consume leaves that intent owed on restart. -/
theorem replay_recovery_unconfirmed (confirmed : List span)
    (suffix : List (ReplayProbe span)) :
    replayRecoveryLog (confirmed.map ReplayProbe.committed ++ .stopped :: suffix) = confirmed := by
  induction confirmed with
  | nil => rfl
  | cons batch rest ih => simp [replayRecoveryLog, ih]

end Lambo.Flush
