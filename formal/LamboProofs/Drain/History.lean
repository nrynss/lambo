/-
# Write-queue drain: the history layer (before #294, #295 and #345)

It holds the layers the Rust has left behind: the lane worker loop before
#295 (`work`, and `close` over it), under which an in-session embedder outage
or lookup timeout answered `failed`, left the intent owed and let the agent's
next write run, with the lane drain it ran to completion (`drainLane`,
`drain`); the replay loop before #294 (`loop`, `replay`, over `replayStep`);
the replay loop over #294's step (`loopWith stepInput`); the replay loop over
#295's first step (`loopD stepFixed`, before an id in another receipt format
was quarantined); and the close before #345, which read no dead letter: the
current worker's `closeW`, in a session where a put was dead-lettered. It
is built and in the axiom audit, but `formal/models.toml` does not check it
for drift and the docs do not cite it. Its theorems are the evidence for the
findings in `Examples.lean`, or state an earlier layer's result; each doc
comment names the current counterpart where there is one.

## Results

* `drain_fifo`, `drain_settles_each`, `work_log`, `drained_final`: a lane
  drained to completion took exactly its submitted writes, in order, and
  every admitted write was settled once with the worker's answer.
* `crash_never_reapplies`: a write in the durable log has its consume beside
  it, so no replay after a crash applies it again, for logs shaped "all puts,
  then worker batches" over an empty intent table. Current:
  `crash_never_reappliesW`.
* `close_settles_all`, `close_answer`, `shutdown_no_silent_loss`: after
  `close()` each admitted write was drained, `intent_durable` and owed, or
  `close_unflushed`. Current: `shutdown_no_silent_lossD`, `held_at_closeW`.
* `outage_failed_but_owed`: an in-session `EmbedUnavailable` answered
  `failed` and left the write owed. Current: `outage_holdsW`.
* `replay_applied_pending`, `replay_drained`, `replay_idempotent`,
  `replay_complete_idem`: the replay applied only unconsumed rows and never
  re-applied a replayed write. Current: `serveR_applied_pending`.
* `replay_order`, `load_seq_order`, `replay_seq_order`,
  `replay_epoch_seq_order`: one serve applied in load order, sequence order
  within a process while no later admission carried an earlier issue time.
  Current: `fixedLoad_sorted`.
* `inputFact_goes_on`, `sessionWide_stops`, `replayStep_sessionWide`,
  `replayStep_inputRefusal_stops`: a fact about the input settled `failed`
  and the replay went on; any other non-transient error, the graph-dependent
  refusals #294 fixed included, stopped it. Current: `stepInput_refusesInput`,
  `stepInput_sessionWide`.
* `fixed_sessionWide_stops`: over #294's loop (`loopWith stepInput`), any
  other non-transient error still stopped the replay. Current:
  `stepInput_sessionWide`.
* `fixedD_input_goes_on`: over `stepFixed`, an input refusal settled `failed`
  and the loop went on with that step, which consumed an id in another
  receipt format `failed` where the current `stepQ` quarantines it.
* `closeW_vouches_dead_letteredW`: before #345 the close vouched for a
  set-aside write whose put a dead letter dropped. Current:
  `closeD_never_vouches_dropped`.

Boundary: a crash in the middle of a replay is not proved.
-/

import LamboProofs.Drain.Proofs.Close

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

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

/-- `spawn_replay` after seeding: nothing owed, nothing to do; otherwise the
liveness embed gates the loop, and a failed one consumes nothing. -/
def replay (live : Bool) (rout : Job → RR) (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loop rout ⟨0, 0⟩ p else { ROut.empty with block := some .embedder }

/-- Drain a lane to completion: pop until the queue is empty. -/
def drainLane {α : Type} : Nat → Lane α → Lane α
  | 0, l => l
  | n + 1, l => drainLane n l.pop

def drain {α : Type} (l : Lane α) : Lane α := drainLane l.queue.length l

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

/-- **A lane drained to completion took its submitted jobs in order, all of
them.** However admissions and worker steps interleaved (`Receipts.run`), a
lane drained to completion has taken exactly the submitted jobs, in
submission order: the completion of `Receipts.lane_fifo`. Only the worker
before #295 drained a lane to completion; the current one takes each lane in
submission order (`Receipts.lane_fifo`, `runW_ord`). -/
theorem drain_fifo (ops : List (Lambo.Receipts.Op Job)) :
    (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).queue = [] ∧
      (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).taken = Lambo.Receipts.submitted ops := by
  obtain ⟨h1, h2⟩ := drainLane_spec _ (Lambo.Receipts.run ⟨[], []⟩ ops) rfl
  refine ⟨h1, ?_⟩
  unfold drain
  rw [h2, Lambo.Receipts.run_history]
  rfl

/-- **With issue times in sequence order, the load is sequence order.** If,
among the rows, a higher sequence number never has an earlier issue time, the
(`issued_ms`, `lane_seq`) order is `lane_seq` order, the admission order the
`WriteIntent::lane_seq` doc promises. A monotone clock alone does not give the
hypothesis: `next_receipt` takes the sequence number before it reads the clock
and before admission takes its lock, so two concurrent admissions can read
the clock in the other order. It holds for sequential admissions under a
clock that does not step back. `clock_step_reorders` is the other side.
`loadOrder` is still the first stage of the current load `fixedLoad`, which
needs no clock assumption (`fixedLoad_sorted`). -/
theorem load_seq_order (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.rid < b.job.rid → a.job.ms ≤ b.job.ms) :
    (loadOrder l).Pairwise (fun a b => a.job.rid ≤ b.job.rid) := by
  apply List.Pairwise.imp_of_mem _ (loadOrder_sorted l)
  intro a b ha hb hab
  rw [keyLe_iff] at hab
  have hm := hmono b (mem_loadOrder.mp hb) a (mem_loadOrder.mp ha)
  omega

/-! ## The worker loop and the close before #295

`work` runs `workerStep` (`Drain/Proofs/Base.lean`) over the jobs a worker
takes; `close` composes it with `settle_deferred`'s set-aside. -/

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

/-! ## The worker before #295

Every theorem in this section is about `workerStep`/`work`, the worker
**before #295** (in-session outage answered `failed`). The current worker's
counterparts: `InvW` and `sessionW_inv` (what each write's receipt holds),
`runW_log_refines` (its batches are `workerStep`'s), in `Drain/Proofs/`. -/

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

/-! ## Draining a lane through the worker before #295 -/

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

/-! ## Close: drained, deferred, or answered `close_unflushed` (worker before #295)

The theorems about `close` (`close_settles_all`, `close_final`,
`close_answer`) are over the worker **before #295**; their current
counterparts are `shutdown_no_silent_lossD`, `held_at_closeW` and
`closeD_retryD`. The lemmas about `deferEntry` and `settleDeferred` they
share with the current close are in `Drain/Proofs/Base.lean`. -/

theorem mem_update {s : RStore} {r : Key} {f : Entry → Entry} {p : Key × Entry}
    (hp : p ∈ update s r f) :
    ∃ q ∈ s, p = (if q.1 = r then (q.1, f q.2) else q) := by
  unfold update at hp
  obtain ⟨q, hq, he⟩ := List.mem_map.mp hp
  exact ⟨q, hq, he.symm⟩

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

/-! ## Replay (before #294 and #295)

`loop` and `replay` over `replayStep`: the replay as it was before #294 and
#295. The current replay is `serveR` over `fixedLoad` (`Drain/Spec/Worker.lean`). -/

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

/-! ## How the replay before #294 settles a refusal

Over the old `loop`; the step's arms that are still current are in
`Drain/Spec/Base.lean`. -/

/-- **Before #294**, any other error that was neither transient nor a fact
about the input stopped `spawn_replay`: a non-cycle store error (another
`Invariant`, a `NotFound`, a backend error), `Config`, a lost lease
(`Conflict`), anything else, and a graph-dependent input refusal
(`Err.inputRefusal`) with them. Since #294 `stepInput` settles that refusal
`failed`; the current statement is `stepInput_sessionWide`. -/
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

/-- The old replay sends an input refusal to the stop arm (pins the code
before #294; `store_error_blocks_backlog` is the backlog it blocks). -/
theorem replayStep_inputRefusal_stops (img : Bool) (s : Streaks) :
    replayStep img (.run (.err .inputRefusal)) s = (.stop .other, s) := rfl

/-! ## How the replay loop over #294's step stops

Over `loopWith stepInput`, which has no deferral and no retry pass; the
current statement is `stepInput_sessionWide`, at the step. -/

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

/-! ## How the replay over #295's first step settles an input refusal

Over `loopD stepFixed`: the current step `stepQ` answers this intent the same
way, but quarantines an id in another receipt format that `stepFixed`
consumes `failed`, so the loop on the rest is not the current replay's. -/

/-- Every input refusal settles `failed` under `stepFixed` too. -/
theorem stepFixed_refusesInput (img : Bool) (e : Err) (s : Streaks) (h : e.refusesInput = true) :
    (stepFixed img (.run (.err e)) s).1 = .failed := by
  rw [stepFixed_agrees img _ s (by simp) (by simp)]
  exact stepInput_refusesInput img e s h

/-- **#294 still held in the loop over `stepFixed`**: an input refusal is
consumed `failed` and the loop goes on with `stepFixed`. No current statement
over `stepQ` yet. -/
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

/-! ## Crash: a durable write is never replayed (worker before #295)

Over `work` and the replay before #294 and #295 (`replay`, `loadOrder`). The
current counterpart, over `runW` and `serveR` with `fixedLoad`, is
`crash_never_reappliesW`. -/

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
counterparts are in `Drain/Spec/Close.lean`: `shutdown_no_silent_lossD`,
`held_at_closeW`, `outage_holdsW`. -/

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
each admitted write is one of (for the current worker, exclusivity is proved
by `shutdown_exactly_oneW`): finished by the drain, with the
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
first-attempt one once a run of them has reached the limit is held. This
theorem is kept because it
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

/-- **#345, in general: the close before the fix vouches for a dead-lettered
put.** In every reachable session, a set-aside write (running, held or
queued when the close began) whose put the history dead-lettered answers
`intent_durable` at a `closeW` whose flush completed, though its put is not
committed. It is here because it reads `closeW` as the close before #345 in
a session with a dead letter, where `Memory::close` is now `closeD`.
Current: `closeD_never_vouches_dropped`. -/
theorem closeW_vouches_dead_letteredW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (hkeys : (all.map (·.key)).Nodup) (hist : List Landed) (now' : Int)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue)
    (hd : j.key ∈ droppedPuts (sessLog all st) hist) :
    ansOf (closeW .durable now' st) j.key = some .intentRecorded ∧
      j.key ∉ committedPuts (sessLog all st) hist := by
  obtain ⟨a, h1, h2⟩ := setAside_unsettled hst hnd hall hj
  exact ⟨(ansOf_closeW_unsettled h1 h2).2,
    fun hc => keysLanded_disjoint _ hist (sessLog_puts_nodup hst hkeys) _ hc hd⟩

end Lambo.Drain
