import LamboProofs.Lease
import LamboProofs.Lib.List

/-
# The flush retry ladder and the store's flush contract

Models `flush_with_retry` (`src/store/flush.rs`) with `BACKOFF_BASE` (100 ms),
`BACKOFF_CAP` (10 s), `RETAINED_BACKOFF` and `next_backoff`;
`StoreError::is_retryable` (`src/types/mod.rs`); and two clauses of the
`GraphStore::flush` contract (`src/store/mod.rs`): the fencing token and
"Idempotency contract (flush replay)". The store is any function from (batch,
attempt) to an outcome; `lostAck` is a commit the loop sees as a retryable
`Backend` error, and `fail` commits nothing (every adapter applies a batch in
one transaction).

Results: the sleeps never decrease and never exceed `BACKOFF_CAP`, equalling
it from the eighth on (`flushWithRetry_backoff`, `backoffAt_capped`); a
ladder makes at most `retries + 1` calls, exactly that many on a retryable
error, all with one batch (`ladder_length_le`, `ladder_exhausted`,
`ladder_same_batch`); only a retryable error is retried, `Constraint` and
`StaleWrite` ending the ladder at once (`ladder_dropLast_retryable`,
`stale_write_one_call`); with the idempotent upsert the contract requires
(`upsert_idempotent`) a successful ladder is applied once in effect, lost
acknowledgements included, and a retained batch re-sent with newer mutations
lands as if sent once (`ladder_ok_effect`, `replay_absorbs`).

Replay consumes use the clock of the successful store attempt for retention.
Receipt retention is outside this retry model.

Boundary: time is milliseconds on the tokio clock and store calls take no
time, so a ladder's duration is the sum of its sleeps; nothing else writes
the store between attempts (`Idempotent`'s assumption). The fence check the
fixed Rust runs before each retry is not modelled (the fence latches only
between wake-ups). The loop around the ladder is `Loop.lean`.
-/

namespace Lambo.Flush

open Lambo.Lib (prefix_drop)

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

/-- From the seventh retry on, `flush_with_retry` sleeps `BACKOFF_CAP`
(`next_backoff` has reached its cap). -/
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

/-- A row upsert (`write_rows`) is idempotent, so re-flushing a batch after a
failed attempt changes no row. -/
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

end Lambo.Flush
