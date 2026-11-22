/-
# Write receipts: outcomes are final, lanes are FIFO, unsettled receipts stay

Model of the receipt store and the per-agent lanes of the asynchronous write
queue (`src/writeq/receipts.rs`, `src/writeq/admission.rs`,
`src/writeq/execution.rs`).

## Results

* `only_two_unsettled`: `Pending` and `PendingReplay` are the only unsettled
  answers ("the other ten are terminal for the process answering").
* `settle_settled`, `settleAll_fixed`: `Entry::settle` never overwrites an
  outcome, so after the first settle the answer is fixed whatever settles are
  attempted later ("no outcome can be overwritten by a later sweep").
* `expire_keeps_unsettled`, `expire_drops_only_old`, `evict_keeps_unsettled`:
  "an unsettled receipt is never expired and never evicted"; what retention
  drops was settled before the cutoff.
* `lookupMissing_*`: an id with no entry gets exactly one of the three specific
  non-answers ("every non-answer is a specific non-answer").
* `run_history`, `lane_fifo`: a lane applies writes in submission order.
* `admit_lane_bound`, `admit_queue_bound`: admission keeps the lane and queue
  within their caps, and a refusal is born settled.

Not modelled: the one settled-to-settled upgrade `CloseUnflushed →
IntentRecorded` on a retried close, which bypasses `Entry::settle` by design
(`drain.rs`); replay; the restart map; the in-serve hold (#295): `hold_one`
answering a held write `pending_replay`, the held writes in `Lanes::held`, and
admission's `DropReason::HeldLane` refusal. `Drain.lean` models the hold
(`hold_then_serve_order`, `sessionW_orderInv`, and the caps in `Caps`).

The admission counts `lane` and `total` below are read as outstanding plus
held writes, as admission counts them (`lane_outstanding + lane_held`,
`outstanding + held_count`).
-/

namespace Lambo.Receipts

/-- `ReceiptAnswer` (`receipts.rs`), without payloads. -/
inductive Answer where
  | pending
  | pendingReplay
  | applied
  | failed
  | appliedAfterRestart
  | intentRecorded
  | closeUnflushed
  | dropped
  | expired
  | restartLost
  | neverIssued
  | forbidden
  deriving DecidableEq, Repr

/-- `is_settled`: "true once the answer can no longer change". -/
def Answer.isSettled : Answer → Bool
  | .pending => false
  | .pendingReplay => false
  | _ => true

/-- Exactly two answers are unsettled (Rust test
`only_the_two_pendings_are_unsettled`). -/
theorem only_two_unsettled (a : Answer) :
    a.isSettled = false ↔ (a = .pending ∨ a = .pendingReplay) := by
  cases a <;> decide

/-! ## Settling -/

/-- A stored receipt entry: its answer, and when it settled. -/
structure Entry where
  answer : Answer
  settledAt : Option Int

/-- `Entry::settle`: refuse if already settled, else record the outcome and
stamp the retention clock. Returns the entry and whether it changed. -/
def settle (e : Entry) (a : Answer) (now : Int) : Entry × Bool :=
  if e.answer.isSettled = true then (e, false)
  else ({ answer := a, settledAt := some now }, true)

/-- A settled entry is returned untouched, with `false`. -/
theorem settle_settled (e : Entry) (a : Answer) (now : Int) (h : e.answer.isSettled = true) :
    settle e a now = (e, false) := by
  unfold settle
  exact ite_eq_left h

theorem settle_unsettled (e : Entry) (a : Answer) (now : Int)
    (h : ¬ e.answer.isSettled = true) :
    settle e a now = ({ answer := a, settledAt := some now }, true) := by
  unfold settle
  exact ite_eq_right h

/-- Settling with a settled answer leaves a settled entry. Workers and the
close path only ever settle with a settled answer (`Applied`, `Failed`,
`IntentRecorded`, `CloseUnflushed`). -/
theorem settle_isSettled (e : Entry) (a : Answer) (now : Int) (ha : a.isSettled = true) :
    (settle e a now).1.answer.isSettled = true := by
  by_cases h : e.answer.isSettled = true
  · rw [settle_settled e a now h]
    exact h
  · rw [settle_unsettled e a now h]
    exact ha

/-- Any number of later settle attempts, in order. -/
def settleAll (e : Entry) : List (Answer × Int) → Entry
  | [] => e
  | (a, now) :: rest => settleAll (settle e a now).1 rest

/-- **Outcomes are final.** Once an entry is settled, no sequence of settle
attempts changes its answer or its settle time. -/
theorem settleAll_fixed : ∀ (attempts : List (Answer × Int)) (e : Entry),
    e.answer.isSettled = true → settleAll e attempts = e := by
  intro attempts
  induction attempts with
  | nil =>
    intro e _
    rfl
  | cons p rest ih =>
    intro e h
    cases p with
    | mk a now =>
      show settleAll (settle e a now).1 rest = e
      rw [settle_settled e a now h]
      exact ih e h

/-- The first settle wins: after one settle with a settled answer, the entry
is whatever that settle produced, forever. -/
theorem first_settle_wins (e : Entry) (a : Answer) (now : Int) (ha : a.isSettled = true)
    (later : List (Answer × Int)) :
    settleAll (settle e a now).1 later = (settle e a now).1 :=
  settleAll_fixed later _ (settle_isSettled e a now ha)

/-! ## Retention

`expire` walks the entries oldest first. An unsettled entry is skipped and
kept; an entry settled before the cutoff is forgotten; the first entry settled
inside the window stops the sweep. `evict` runs while the store is over
capacity: it skips unsettled entries and forgets settled ones. -/

/-- `Receipts::expire` for a given `cutoff = now − RECEIPT_RETENTION`. -/
def expire (cutoff : Int) : List Entry → List Entry
  | [] => []
  | e :: rest =>
    match e.settledAt with
    | none => e :: expire cutoff rest
    | some t => if t < cutoff then expire cutoff rest else e :: rest

/-- **An unsettled receipt is never expired.** -/
theorem expire_keeps_unsettled (cutoff : Int) :
    ∀ (entries : List Entry) (e : Entry), e ∈ entries → e.settledAt = none →
      e ∈ expire cutoff entries := by
  intro entries
  induction entries with
  | nil =>
    intro e he _
    simp at he
  | cons x rest ih =>
    intro e he hnone
    cases hx : x.settledAt with
    | none =>
      have hstep : expire cutoff (x :: rest) = x :: expire cutoff rest := by
        simp [expire, hx]
      rw [hstep]
      cases List.mem_cons.mp he with
      | inl heq => exact List.mem_cons.mpr (Or.inl heq)
      | inr hmem => exact List.mem_cons.mpr (Or.inr (ih e hmem hnone))
    | some t =>
      by_cases hold : t < cutoff
      · have hstep : expire cutoff (x :: rest) = expire cutoff rest := by
          simp [expire, hx, hold]
        rw [hstep]
        cases List.mem_cons.mp he with
        | inl heq =>
          rw [heq, hx] at hnone
          cases hnone
        | inr hmem => exact ih e hmem hnone
      · have hstep : expire cutoff (x :: rest) = x :: rest := by
          simp [expire, hx, hold]
        rw [hstep]
        exact he

/-- Everything `expire` forgets was settled strictly before the cutoff: an
entry still inside its retention window is kept. -/
theorem expire_drops_only_old (cutoff : Int) :
    ∀ (entries : List Entry) (e : Entry), e ∈ entries →
      e ∈ expire cutoff entries ∨ ∃ t, e.settledAt = some t ∧ t < cutoff := by
  intro entries
  induction entries with
  | nil =>
    intro e he
    simp at he
  | cons x rest ih =>
    intro e he
    cases hx : x.settledAt with
    | none =>
      have hstep : expire cutoff (x :: rest) = x :: expire cutoff rest := by
        simp [expire, hx]
      rw [hstep]
      cases List.mem_cons.mp he with
      | inl heq => exact Or.inl (List.mem_cons.mpr (Or.inl heq))
      | inr hmem =>
        cases ih e hmem with
        | inl h => exact Or.inl (List.mem_cons.mpr (Or.inr h))
        | inr h => exact Or.inr h
    | some t =>
      by_cases hold : t < cutoff
      · have hstep : expire cutoff (x :: rest) = expire cutoff rest := by
          simp [expire, hx, hold]
        rw [hstep]
        cases List.mem_cons.mp he with
        | inl heq =>
          refine Or.inr ⟨t, ?_, hold⟩
          rw [heq]
          exact hx
        | inr hmem => exact ih e hmem
      · have hstep : expire cutoff (x :: rest) = x :: rest := by
          simp [expire, hx, hold]
        rw [hstep]
        exact Or.inl he

/-- `Receipts::evict`: drop up to `excess` settled entries, oldest first,
skipping unsettled ones. -/
def evict : Nat → List Entry → List Entry
  | 0, entries => entries
  | _ + 1, [] => []
  | excess + 1, e :: rest =>
    match e.settledAt with
    | none => e :: evict (excess + 1) rest
    | some _ => evict excess rest

/-- **An unsettled receipt is never evicted**, however far over capacity the
store is (Rust test
`eviction_is_oldest_settled_first_and_never_takes_a_running_writes_receipt`). -/
theorem evict_keeps_unsettled :
    ∀ (entries : List Entry) (excess : Nat) (e : Entry), e ∈ entries →
      e.settledAt = none → e ∈ evict excess entries := by
  intro entries
  induction entries with
  | nil =>
    intro excess e he _
    simp at he
  | cons x rest ih =>
    intro excess e he hnone
    cases excess with
    | zero =>
      have hstep : evict 0 (x :: rest) = x :: rest := by simp [evict]
      rw [hstep]
      exact he
    | succ n =>
      cases hx : x.settledAt with
      | none =>
        have hstep : evict (n + 1) (x :: rest) = x :: evict (n + 1) rest := by
          simp [evict, hx]
        rw [hstep]
        cases List.mem_cons.mp he with
        | inl heq => exact List.mem_cons.mpr (Or.inl heq)
        | inr hmem => exact List.mem_cons.mpr (Or.inr (ih (n + 1) e hmem hnone))
      | some t =>
        have hstep : evict (n + 1) (x :: rest) = evict n rest := by
          simp [evict, hx]
        rw [hstep]
        cases List.mem_cons.mp he with
        | inl heq =>
          rw [heq, hx] at hnone
          cases hnone
        | inr hmem => exact ih n e hmem hnone

/-! ## Looking up an id that has no entry -/

/-- The tail of `lookup`: the id is in neither the live store nor the restart
map. -/
def lookupMissing (sameEpoch : Bool) (seq highestSeq : Nat) : Answer :=
  if sameEpoch = false then .restartLost
  else if highestSeq < seq then .neverIssued
  else .expired

/-- An id from another process epoch is `restart_lost`. -/
theorem lookupMissing_restartLost (seq highestSeq : Nat) :
    lookupMissing false seq highestSeq = .restartLost := by
  unfold lookupMissing
  exact ite_eq_left rfl

/-- An id of this epoch above the highest issued sequence is `never_issued`. -/
theorem lookupMissing_neverIssued (seq highestSeq : Nat) (h : highestSeq < seq) :
    lookupMissing true seq highestSeq = .neverIssued := by
  have hne : ¬ (true = false) := by decide
  unfold lookupMissing
  rw [ite_eq_right hne, ite_eq_left h]

/-- An id of this epoch that was issued is `expired`: it existed and its
retention ended. -/
theorem lookupMissing_expired (seq highestSeq : Nat) (h : seq ≤ highestSeq) :
    lookupMissing true seq highestSeq = .expired := by
  have hn : ¬ highestSeq < seq := by omega
  have hne : ¬ (true = false) := by decide
  unfold lookupMissing
  rw [ite_eq_right hne, ite_eq_right hn]

/-- The answer for a missing id is always settled: a caller waiting on it is
never told to keep waiting. -/
theorem lookupMissing_isSettled (sameEpoch : Bool) (seq highestSeq : Nat) :
    (lookupMissing sameEpoch seq highestSeq).isSettled = true := by
  cases sameEpoch with
  | false => rw [lookupMissing_restartLost] <;> rfl
  | true =>
    by_cases h : highestSeq < seq
    · rw [lookupMissing_neverIssued seq highestSeq h] <;> rfl
    · have hle : seq ≤ highestSeq := by omega
      rw [lookupMissing_expired seq highestSeq hle] <;> rfl

/-! ## Lanes are FIFO

`Lanes.queues` maps each agent to a `VecDeque<Job>`; admission does
`push_back`, the lane's single worker does `pop_front`. -/

/-- One agent's lane: the queued jobs, and the jobs the worker has taken, in
the order it took them. -/
structure Lane (α : Type) where
  queue : List α
  taken : List α

/-- A lane operation. -/
inductive Op (α : Type) where
  | push (job : α)
  | pop

variable {α : Type}

/-- `push_back`. -/
def Lane.push (l : Lane α) (job : α) : Lane α :=
  { queue := l.queue ++ [job], taken := l.taken }

/-- `pop_front`; nothing happens on an empty queue. -/
def Lane.pop (l : Lane α) : Lane α :=
  match l.queue with
  | [] => l
  | job :: rest => { queue := rest, taken := l.taken ++ [job] }

/-- Everything submitted to the lane so far: taken jobs, then queued ones. -/
def Lane.history (l : Lane α) : List α := l.taken ++ l.queue

theorem Lane.push_history (l : Lane α) (job : α) :
    (l.push job).history = l.history ++ [job] := by
  show l.taken ++ (l.queue ++ [job]) = l.taken ++ l.queue ++ [job]
  exact (List.append_assoc l.taken l.queue [job]).symm

theorem Lane.pop_history (l : Lane α) : l.pop.history = l.history := by
  cases l with
  | mk queue taken =>
    cases queue with
    | nil => rfl
    | cons job rest =>
      show taken ++ [job] ++ rest = taken ++ (job :: rest)
      simp

/-- Run a sequence of operations. -/
def run : Lane α → List (Op α) → Lane α
  | l, [] => l
  | l, Op.push job :: ops => run (l.push job) ops
  | l, Op.pop :: ops => run l.pop ops

/-- The jobs a sequence of operations submits, in order. -/
def submitted : List (Op α) → List α
  | [] => []
  | Op.push job :: ops => job :: submitted ops
  | Op.pop :: ops => submitted ops

/-- Whatever the interleaving of submissions and worker steps, taken-then-queued
is the submission order. -/
theorem run_history : ∀ (ops : List (Op α)) (l : Lane α),
    (run l ops).history = l.history ++ submitted ops := by
  intro ops
  induction ops with
  | nil =>
    intro l
    simp [run, submitted]
  | cons op rest ih =>
    intro l
    cases op with
    | push job =>
      show (run (l.push job) rest).history = l.history ++ (job :: submitted rest)
      rw [ih (l.push job), Lane.push_history] <;> simp
    | pop =>
      show (run l.pop rest).history = l.history ++ submitted rest
      rw [ih l.pop, Lane.pop_history]

/-- **A lane applies in submission order.** Starting from an empty lane, the
jobs the worker has taken are a prefix of the jobs submitted, in the same
order: no job overtakes an earlier one of the same agent. -/
theorem lane_fifo (ops : List (Op α)) :
    ∃ stillQueued,
      (run { queue := [], taken := [] } ops).taken ++ stillQueued = submitted ops := by
  have h := run_history ops ({ queue := [], taken := [] } : Lane α)
  refine ⟨(run { queue := [], taken := [] } ops).queue, ?_⟩
  have e : (({ queue := [], taken := [] } : Lane α)).history = [] := rfl
  rw [e] at h
  simpa [Lane.history] using h

/-! ## Admission bounds

Admission refuses a write when its agent's lane holds `WRITE_QUEUE_LANE_MAX`
outstanding and held jobs or the queue holds `WRITE_QUEUE_MAX`, and otherwise
enqueues it. Admission never waits: a refusal is answered at once with a `Dropped`
receipt. -/

/-- `MAX_RETAINED_RECEIPTS`. -/
def maxRetainedReceipts : Nat := 4096

/-- `WRITE_QUEUE_MAX = MAX_RETAINED_RECEIPTS / 4`. -/
def writeQueueMax : Nat := maxRetainedReceipts / 4

/-- `WRITE_QUEUE_LANE_MAX = WRITE_QUEUE_MAX / MAX_CONCURRENT_RECEIPT_WAITS`,
with 16 concurrent waits. -/
def writeQueueLaneMax : Nat := writeQueueMax / 16

theorem writeQueueMax_eq : writeQueueMax = 1024 := by decide

theorem writeQueueLaneMax_eq : writeQueueLaneMax = 64 := by decide

/-- Every job that can be outstanding has room for its receipt: the retained
receipt store is four times the queue bound, so eviction of settled entries
alone can always make room. -/
theorem queue_fits_in_retention : 4 * writeQueueMax ≤ maxRetainedReceipts := by decide

/-- The lane and queue counts after an admission attempt, and whether it was
accepted. -/
def tryAdmit (lane total : Nat) : Nat × Nat × Bool :=
  if writeQueueLaneMax ≤ lane then (lane, total, false)
  else if writeQueueMax ≤ total then (lane, total, false)
  else (lane + 1, total + 1, true)

/-- Admission keeps a lane within `WRITE_QUEUE_LANE_MAX`. -/
theorem admit_lane_bound (lane total : Nat) (h : lane ≤ writeQueueLaneMax) :
    (tryAdmit lane total).1 ≤ writeQueueLaneMax := by
  unfold tryAdmit
  by_cases h1 : writeQueueLaneMax ≤ lane
  · rw [ite_eq_left h1]
    exact h
  · rw [ite_eq_right h1]
    by_cases h2 : writeQueueMax ≤ total
    · rw [ite_eq_left h2]
      exact h
    · rw [ite_eq_right h2]
      show lane + 1 ≤ writeQueueLaneMax
      omega

/-- Admission keeps the queue within `WRITE_QUEUE_MAX`. -/
theorem admit_queue_bound (lane total : Nat) (h : total ≤ writeQueueMax) :
    (tryAdmit lane total).2.1 ≤ writeQueueMax := by
  unfold tryAdmit
  by_cases h1 : writeQueueLaneMax ≤ lane
  · rw [ite_eq_left h1]
    exact h
  · rw [ite_eq_right h1]
    by_cases h2 : writeQueueMax ≤ total
    · rw [ite_eq_left h2]
      exact h
    · rw [ite_eq_right h2]
      show total + 1 ≤ writeQueueMax
      omega

/-- A refusal changes neither count: "a refusal never enters `accepted`". -/
theorem admit_refused_unchanged (lane total : Nat) (h : (tryAdmit lane total).2.2 = false) :
    (tryAdmit lane total).1 = lane ∧ (tryAdmit lane total).2.1 = total := by
  unfold tryAdmit at h ⊢
  by_cases h1 : writeQueueLaneMax ≤ lane
  · rw [ite_eq_left h1]
    exact ⟨rfl, rfl⟩
  · rw [ite_eq_right h1] at h ⊢
    by_cases h2 : writeQueueMax ≤ total
    · rw [ite_eq_left h2]
      exact ⟨rfl, rfl⟩
    · rw [ite_eq_right h2] at h
      have h' : true = false := h
      exact absurd h' (by decide)

/-- The receipt of a refused write is born settled (`Dropped`), so a caller
that waits on it returns at once. -/
theorem dropped_is_settled : Answer.dropped.isSettled = true := rfl

end Lambo.Receipts
