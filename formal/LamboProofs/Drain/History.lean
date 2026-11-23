/-
# Write-queue drain: the history layer (before #294 and #295)

**Nothing in this file describes the current Rust.** It holds the lane worker
loop as it was before #295 (`work`, and `close` over it), under which an
in-session embedder outage (or lookup timeout) answered `failed`, left the
intent owed and let the agent's next write run; and the theorems over the
replay loop before #294 and #295 (`replay`, `loop` over `replayStep`). The
pieces the current model shares with them (`workerStep`, the receipt store,
`settleDeferred`, the lane drain, the `Drop` path, the restart map's seed,
`replayStep` and its arms, `loop`, `replay`) are defined, with their
theorems, in `Base.lean`. The file is built and its theorems are in the axiom
audit, but `formal/models.toml` does not check it against the Rust for drift,
and the docs do not cite it. Its theorems were the evidence for the original
findings (`Examples.lean`), so they stay. The current counterparts are named
in each doc comment.

## Results (before #295)

* `drain_settles_each`, `work_log`, `drained_final`: every admitted write is
  settled once with the worker's answer (`applied` or `failed`), one batch per
  job, and no later settle changes it.
* `crash_never_reapplies`: a write in the durable log has its consume beside
  it (`workerStep_atomic`), so no replay after a crash applies it again
  (`Graph::consume_write_intent`: "the double-apply this design excludes").
  Proved for logs shaped "all puts, then worker batches": the Rust relies on
  the admission lock order (graph write, then lanes) to put each job's put
  before its consume, and the model takes that shape as given. A crash in the
  middle of a replay is not proved (the replay's own batches have the same
  write-with-consume shape, `loop_facts`). Like `shutdown_no_silent_loss`, it
  is about one process's log over an empty intent table: an earlier epoch's
  backlog is not in the statement. Current counterpart:
  `crash_never_reappliesW`.
* `close_settles_all`, `close_answer`, `shutdown_no_silent_loss`: after
  `close()` every receipt is settled, and each admitted write is drained,
  `intent_durable` and owed by the next replay, or `close_unflushed`
  ("deferred, not lost", `quiesce`). Current counterparts:
  `shutdown_no_silent_lossW`, `held_at_closeW`.
* `outage_failed_but_owed`: an in-session `EmbedUnavailable` answered
  `failed` and left the write owed. The current Rust holds the write
  (`pending_replay`), `outage_holdsW`.
* `replay_applied_pending`, `replay_drained`, `replay_idempotent`,
  `replay_complete_idem`: the replay before #294 applied only unconsumed
  rows; replaying a fully drained queue changed nothing; a replayed write was
  never applied by a later replay. Current counterpart:
  `serveR_applied_pending`.
* `replay_order`, `replay_seq_order`, `replay_epoch_seq_order`: one serve
  applied in load order, which within one process is sequence (admission)
  order as long as no later admission carries an earlier issue time.
* `inputFact_goes_on`: a refusal that is a fact about the intent's input
  (`Embed`, `ImageIdTaken`, `Reference`, a cycle refusal, an undecodable
  payload) settled `failed` and the replay went on (`StoreError::is_cycle_refusal`
  doc; test `a_cycle_refused_at_replay_settles_failed_and_the_replay_goes_on`).
  `sessionWide_stops`: any other non-transient error stopped it, the intent
  and the rest left unconsumed. `replayStep_inputRefusal_stops`: the
  graph-dependent refusal #294 fixed, and `replayStep_sessionWide`, the step
  arm that stopped on it. The step's other arms (`replayStep_cycle`,
  `replayStep_decode`, `unavailable_skips`, ...) are still the current
  step's and are in `Base.lean`. Current counterparts: `fixed_input_goes_on`,
  `fixed_sessionWide_stops` (#294) and `fixedD_input_goes_on` (#295).

Boundary: this is the Rust before #294 and #295, not checked for drift.
`crash_never_reapplies` is proved for logs shaped "all puts, then worker
batches" over an empty intent table; a crash in the middle of a replay is not
proved.
-/

import LamboProofs.Drain.Base

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The worker loop and the close before #295

`work` runs `Base.lean`'s `workerStep` over the jobs a worker takes; `close`
composes it with `settle_deferred`'s set-aside. -/

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
`runW_log_refines` (its batches are `workerStep`'s). `workerStep_keys` and
`workerStep_atomic`, which the current worker's theorems use, are in
`Base.lean`. -/

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

/-! ## Draining a lane through the worker before #295

`drain` and `drain_fifo` (the lane's own order) are in `Base.lean`. -/

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
counterparts are `shutdown_no_silent_lossW`, `held_at_closeW` and
`closeW_retry_upgradesW`. The lemmas about `deferEntry` and
`settleDeferred` they share with the current close are in `Base.lean`. -/

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

`loop` and `replay` over `replayStep` (`Base.lean`): the replay as it was
before #294 and #295. The current replay is `serveR` over `fixedLoad`
(`Replay.lean`, `Worker.lean`). -/

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
`Base.lean`. -/

/-- **Before #294**, any other error that was neither transient nor a fact
about the input stopped `spawn_replay`: a non-cycle store error (another
`Invariant`, a `NotFound`, a backend error), `Config`, a lost lease
(`Conflict`), anything else, and a graph-dependent input refusal
(`Err.inputRefusal`) with them. Since #294 `stepInput` settles that refusal
`failed`; the current statement is `stepInput_sessionWide` and, over the
loop, `fixed_sessionWide_stops`. -/
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
counterparts are in `Close.lean`: `shutdown_no_silent_lossW`, `held_at_closeW`,
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

end Lambo.Drain
