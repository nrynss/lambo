import LamboProofs.Flush.Loop

/-
# The flush's error classes (#301 item 3, `src/store/error.rs`)

Models how the adapters classify a failed flush (`ErrorClass`, `map_write_err`
in `src/store/error.rs`): retryable, constraint, stale, and permanent but not a
constraint (`Cls`), the store as the loop sees it through a mapping
(`CStore.via`), and the loop after the fix (`settleClassed`, `runClassed`,
`runClassedGuarded`), over `Loop.lean`; and the concrete runs of the flush
model.

Results: before the fix a permanent error reached the loop as `Backend`, so it
was retried and retained and resent until the backlog degraded the session
(`permanent_retryable_old`, `permanent_retried_retained`). With
`StoreError::Permanent` a permanent error costs one call, degrades the session
and nothing calls the store again, and `close` reports an error
(`permanent_one_call`, `permanent_degrades_classed`, `close_not_ok_degraded`);
every other class is handled as before (`via_fixed_agrees`,
`stepClassed_agrees`) and the invariants hold (`no_silent_loss_classed`,
`close_ok_accounted_classed`). After review the permanent class is split by
scope: a value the store rejects is dead-lettered as a constraint violation
while the session goes on (`value_rejected_dead_letters`), an error about the
store itself degrades (`store_permanent_degrades_split`), and the results
carry over (`no_silent_loss_split`).

Boundary: which SQLSTATE or SQLite code falls in which class is the adapters'
table, stated in the doc comments, not derived; the classification is an
input to the loop.
-/

namespace Lambo.Flush

open Lambo.Lib (prefix_drop)

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

/-- Splitting the permanent error class by scope leaves a retryable store
error retryable (`StoreError::is_retryable`). -/
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
