/-
# Write-queue drain: the close, crash and shutdown over the current worker

The close (`closeW`, `closeD`) after any trace of `Worker.lean`'s current
worker, a crash and the serve that follows it, the dead-lettered batches at
close (#345), worker panics and the dead-lettered set, and where the
lookup-timeout streak resets (#346).

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

Boundary: each step is atomic, as in `Base.lean`; which puts the final flush
committed is an input (`Landed`), the flush loop itself being `Flush/`; the
lease fence at the flush is not modelled.
-/

import LamboProofs.Drain.Worker

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

/-- Every write the workers hold: settled, running, held, queued. -/
def DW (st : WS) : List Job := st.settled.map (·.1) ++ st.running ++ st.held ++ st.queue

/-! ### Distinct receipt ids across the workers' lists -/

theorem nodup_of_agents : ∀ (l : List Job), (∀ b, (ofAgent b l).Nodup) → l.Nodup := by
  intro l
  induction l with
  | nil => intro _; exact List.nodup_nil
  | cons x xs ih =>
    intro h
    have hx := h x.agent
    rw [ofAgent_cons, ite_eq_left rfl, List.nodup_cons] at hx
    refine List.nodup_cons.mpr ⟨?_, ih ?_⟩
    · intro hm; exact hx.1 (List.mem_filter.mpr ⟨hm, by simp⟩)
    · intro b
      have hb := h b
      rw [ofAgent_cons] at hb
      split at hb
      · exact (List.nodup_cons.mp hb).2
      · exact hb

theorem nodup_keys_sub {m : List Job} (hk : (m.map (·.key)).Nodup) :
    ∀ {l : List Job}, l.Nodup → (∀ x ∈ l, x ∈ m) → (l.map (·.key)).Nodup := by
  intro l
  induction l with
  | nil => intro _ _; exact List.nodup_nil
  | cons x xs ih =>
    intro hl hm
    rw [List.nodup_cons] at hl
    rw [List.map_cons, List.nodup_cons]
    refine ⟨?_, ih hl.2 (fun y hy => hm y (List.mem_cons_of_mem _ hy))⟩
    intro hx
    obtain ⟨y, hy, hyx⟩ := List.mem_map.mp hx
    have := eq_of_key hk (hm y (List.mem_cons_of_mem _ hy)) (hm x List.mem_cons_self) hyx
    exact hl.1 (this ▸ hy)

theorem dropFirst_perm (a : Nat) : ∀ (l : List Job) (h : Job) (t : List Job),
    ofAgent a l = h :: t → (h :: dropFirst a l).Perm l := by
  intro l
  induction l with
  | nil => intro h t hl; simp [ofAgent_nil] at hl
  | cons x xs ih =>
    intro h t hl
    rw [ofAgent_cons] at hl
    by_cases hx : x.agent = a
    · simp only [hx, ite_true, List.cons.injEq] at hl
      obtain ⟨rfl, rfl⟩ := hl
      simp only [dropFirst, hx, ite_true]
      exact List.Perm.refl _
    · simp only [hx, ite_false] at hl
      simp only [dropFirst, hx, ite_false]
      exact (List.Perm.swap x h _).trans ((ih h t hl).cons x)

theorem mem_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t)
    {x : Job} (hx : x ∈ dropFirst a l) : x ∈ l :=
  (dropFirst_perm a l h t hl).mem_iff.mp (List.mem_cons_of_mem _ hx)

theorem head_mem {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t) :
    h ∈ l :=
  (mem_ofAgent (by rw [hl]; exact List.mem_cons_self)).1

theorem key_ne_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job}
    (hl : ofAgent a l = h :: t) (hk : (l.map (·.key)).Nodup) {x : Job} (hx : x ∈ dropFirst a l) :
    x.key ≠ h.key := by
  have hp := (dropFirst_perm a l h t hl).map (·.key)
  have hn := (List.nodup_cons.mp (hp.nodup_iff.mpr hk)).1
  intro he
  exact hn (by show h.key ∈ _; rw [← he]; exact List.mem_map_of_mem hx)

/-- What distinct receipt ids give: the four lists' ids are distinct, within
and across lists, and every write in them was admitted. -/
structure KeysW (st : WS) : Prop where
  run : (st.running.map (·.key)).Nodup
  held : (st.held.map (·.key)).Nodup
  queue : (st.queue.map (·.key)).Nodup
  sr : ∀ p ∈ st.settled, ∀ y ∈ st.running, p.1.key ≠ y.key
  sh : ∀ p ∈ st.settled, ∀ y ∈ st.held, p.1.key ≠ y.key
  sq : ∀ p ∈ st.settled, ∀ y ∈ st.queue, p.1.key ≠ y.key
  rh : ∀ y ∈ st.running, ∀ z ∈ st.held, y.key ≠ z.key
  rq : ∀ y ∈ st.running, ∀ z ∈ st.queue, y.key ≠ z.key
  hq : ∀ y ∈ st.held, ∀ z ∈ st.queue, y.key ≠ z.key
  adm : ∀ x ∈ DW st, x ∈ st.admitted

theorem mem_DW_of_admitted {st : WS} (hO : OrdW st) {j : Job} (hj : j ∈ st.admitted) :
    j ∈ DW st := by
  have h1 : j ∈ ofAgent j.agent st.admitted := List.mem_filter.mpr ⟨hj, by simp⟩
  rw [← (hO j.agent).1] at h1
  simp only [DW] at h1 ⊢
  simp only [List.mem_append] at h1 ⊢
  rcases h1 with ((h | h) | h) | h
  · exact Or.inl (Or.inl (Or.inl (mem_ofAgent h).1))
  · exact Or.inl (Or.inl (Or.inr (mem_ofAgent h).1))
  · exact Or.inl (Or.inr (mem_ofAgent h).1)
  · exact Or.inr (mem_ofAgent h).1

theorem keysW_of (st : WS) (hO : OrdW st) (hnd : (st.admitted.map (·.key)).Nodup) : KeysW st := by
  have hadm : st.admitted.Nodup :=
    List.Pairwise.of_map (·.key) (fun _ _ h e => h (e ▸ rfl)) hnd
  have hsub : ∀ x ∈ DW st, x ∈ st.admitted := by
    intro x hx
    have h1 : x ∈ ofAgent x.agent (DW st) := List.mem_filter.mpr ⟨hx, by simp⟩
    have h2 : ofAgent x.agent (DW st) = ofAgent x.agent st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO x.agent).1
    rw [h2] at h1
    exact (mem_ofAgent h1).1
  have hD : (DW st).Nodup := by
    apply nodup_of_agents
    intro b
    have h2 : ofAgent b (DW st) = ofAgent b st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO b).1
    rw [h2]
    exact hadm.sublist List.filter_sublist
  have hK := nodup_keys_sub hnd hD hsub
  unfold DW at hK
  rw [List.map_append, List.map_append, List.map_append] at hK
  obtain ⟨h1, hQ, x1⟩ := List.nodup_append.mp hK
  obtain ⟨h2, hH, x2⟩ := List.nodup_append.mp h1
  obtain ⟨_, hR, x3⟩ := List.nodup_append.mp h2
  have mS : ∀ p ∈ st.settled, p.1.key ∈ (st.settled.map (·.1)).map (·.key) :=
    fun p hp => List.mem_map_of_mem (List.mem_map_of_mem hp)
  have mk : ∀ {l : List Job} {y : Job}, y ∈ l → y.key ∈ l.map (·.key) :=
    fun hy => List.mem_map_of_mem hy
  refine ⟨hR, hH, hQ, ?_, ?_, ?_, ?_, ?_, ?_, hsub⟩
  · intro p hp y hy; exact x3 _ (mS p hp) _ (mk hy)
  · intro p hp y hy; exact x2 _ (List.mem_append_left _ (mS p hp)) _ (mk hy)
  · intro p hp y hy
    exact x1 _ (List.mem_append_left _ (List.mem_append_left _ (mS p hp))) _ (mk hy)
  · intro y hy z hz; exact x2 _ (List.mem_append_right _ (mk hy)) _ (mk hz)
  · intro y hy z hz
    exact x1 _ (List.mem_append_left _ (List.mem_append_right _ (mk hy))) _ (mk hz)
  · intro y hy z hz; exact x1 _ (List.mem_append_right _ (mk hy)) _ (mk hz)

/-! ### The receipts and the log, as the workers leave them -/

theorem ansOf_settleOne_ne {s : RStore} {k q : Key} (x : Answer) (now : Int) (h : q ≠ k) :
    ansOf (settleOne s k x now) q = ansOf s q := by
  unfold ansOf settleOne; rw [lookup_update_ne s _ h]

theorem ansOf_settleOne_self {s : RStore} {k : Key} {a : Answer} (x : Answer) (now : Int)
    (ha : ansOf s k = some a) (hu : a.isSettled = false) :
    ansOf (settleOne s k x now) k = some x := by
  unfold ansOf at ha ⊢
  unfold settleOne
  rw [lookup_update, ite_eq_left rfl]
  cases hl : lookup s k with
  | none => rw [hl] at ha; simp at ha
  | some e =>
    rw [hl] at ha
    have he : e.answer = a := by simpa using ha
    have hs : ¬ e.answer.isSettled = true := by rw [he, hu]; decide
    simp [Lambo.Receipts.settle_unsettled e x now hs]

theorem untouched_snoc {q k : Key} {log : List Batch} {b : Batch} (hq : q ≠ k)
    (hb : ∀ m ∈ b, m = .write k ∨ ∃ t, m = .consume k t) (hu : Untouched q log) :
    Untouched q (log ++ [b]) := by
  intro t ht
  rw [List.flatten_append, List.mem_append] at ht
  cases ht with
  | inl h => exact hu t h
  | inr h =>
    simp only [List.flatten_cons, List.flatten_nil, List.append_nil] at h
    rcases hb _ h with h' | ⟨t', h'⟩
    · cases h'
    · exact hq (Mut.consume.inj h').1

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

theorem invW_init (all : List Job) : InvW all (WS.init (admitStore all)) := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp; simp [WS.init] at hp
  · intro j hj; simp [WS.init] at hj
  · intro j hj; simp [WS.init] at hj
  · intro j hj _
    obtain ⟨e, he, ha⟩ := lookup_admit hj
    refine ⟨show ansOf (admitStore all) j.key = _ by simp [ansOf, he, ha], ?_⟩
    intro t ht; simp [WS.init] at ht

/-! The moves, one at a time. -/

theorem takeHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job}
    (hI : InvW all st) (hh : ofAgent a st.held = h :: t) : InvW all (takeHeld a h st) := by
  refine ⟨hI.settled, ?_, ?_, hI.fresh⟩
  · intro j hj
    simp only [takeHeld, List.mem_append, List.mem_singleton] at hj
    rcases hj with (hj | hj) | hj
    · exact hI.live j (List.mem_append_left _ hj)
    · subst hj
      obtain ⟨h1, h2⟩ := hI.held _ (head_mem hh)
      exact ⟨⟨_, h1, rfl⟩, h2⟩
    · exact hI.live j (List.mem_append_right _ hj)
  · intro j hj; exact hI.held j (mem_dropFirst hh hj)

theorem takeQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job}
    (hI : InvW all st) (hq : ofAgent a st.queue = q :: t) : InvW all (takeQueued a q st) := by
  refine ⟨hI.settled, ?_, hI.held, hI.fresh⟩
  intro j hj
  simp only [takeQueued, List.mem_append, List.mem_singleton] at hj
  rcases hj with (hj | hj) | hj
  · exact hI.live j (List.mem_append_left _ hj)
  · subst hj; exact hI.live _ (List.mem_append_right _ (head_mem hq))
  · exact hI.live j (List.mem_append_right _ (mem_dropFirst hq hj))

theorem parkQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hq : ofAgent a st.queue = q :: t) :
    InvW all (parkQueued now a q st) := by
  have hqm := head_mem hq
  have hqA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hqm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hqm)]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hqm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append, List.mem_singleton] at hj
    rcases hj with hj | hj
    · obtain ⟨h1, h2⟩ := hI.held j hj
      exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hqm)]; exact h1, h2⟩
    · subst hj
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live _ (List.mem_append_right _ hqm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hqA)
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hh : ofAgent a st.held = h :: t) :
    InvW all (refuseHeld now a h st) := by
  have hm := head_mem hh
  have hA : h.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm h (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseHeld, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (hK.sh p hp h hm)]; exact h1, h2⟩
    · subst hp
      obtain ⟨h1, h2⟩ := hI.held h hm
      exact ⟨ansOf_settleOne_self _ now h1 rfl, Or.inr (Or.inr ⟨rfl, hf, h2⟩)⟩
  · intro j hj
    have hne : j.key ≠ h.key := by
      rcases List.mem_append.mp hj with hj | hj
      · exact hK.rh j hj h hm
      · exact fun e => hK.hq h hm j hj e.symm
    obtain ⟨h1, h2⟩ := hI.live j hj
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    have hj' : j ∈ dropFirst a st.held := hj
    obtain ⟨h1, h2⟩ := hI.held j (mem_dropFirst hh hj')
    exact ⟨by
      simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (key_ne_dropFirst hh hK.held hj')]; exact h1,
      h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ h.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hq : ofAgent a st.queue = q :: t) :
    InvW all (refuseQueued now a q st) := by
  have hm := head_mem hq
  have hA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseQueued, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hm)]; exact h1,
        h2⟩
    · subst hp
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live q (List.mem_append_right _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, Or.inr (Or.inr ⟨rfl, hf, h3⟩)⟩
  · intro j hj
    simp only [refuseQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    obtain ⟨h1, h2⟩ := hI.held j hj
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hm)]; exact h1, h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem settleRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (x : Answer) (b : Batch) (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t)
    (hxb : (x = .applied ∧ b = [.write j.key, .consume j.key .applied]) ∨
      (x = .failed ∧ b = [.consume j.key .failed])) :
    InvW all (settleRun now a j x b st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  have hb : ∀ m ∈ b, m = .write j.key ∨ ∃ t, m = .consume j.key t := by
    rcases hxb with ⟨_, rfl⟩ | ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      rcases hm' with rfl | rfl
      · exact Or.inl rfl
      · exact Or.inr ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      subst hm'; exact Or.inr ⟨_, rfl⟩
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [settleRun, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      have hne := hK.sr p hp j hm
      refine ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, ?_⟩
      simp only [settleRun]
      rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
      · exact Or.inl ⟨e1, List.mem_append_left _ e2⟩
      · exact Or.inr (Or.inl ⟨e1, List.mem_append_left _ e2⟩)
      · exact Or.inr (Or.inr ⟨e1, e2, untouched_snoc hne hb e3⟩)
    · subst hp
      obtain ⟨⟨y, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ hm)
      refine ⟨ansOf_settleOne_self _ now h1 h2, ?_⟩
      simp only [settleRun]
      rcases hxb with ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩
      · exact Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩
      · exact Or.inr (Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩)
  · intro y hy
    simp only [settleRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy
    have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
    obtain ⟨h1, h2⟩ := hI.held y hy
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩

theorem holdRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t) :
    InvW all (holdRun now a j st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ (hK.sr p hp j hm)]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_cons] at hy
    rcases hy with rfl | hy
    · obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live y (List.mem_append_left _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
    · have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
      obtain ⟨h1, h2⟩ := hI.held y hy
      exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem startW_inv (now : Int) (a : Nat) {all : List Job} {st : WS} (hI : InvW all st)
    (hK : KeysW st) : InvW all (startW now a st) := by
  unfold startW
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hI
  · rw [ite_eq_right hr]
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hI
      | cons q t =>
        simp only
        split
        · next hf => exact refuseQueued_inv now hI hK hf hq
        · exact takeQueued_inv hI hq
    | cons h t =>
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact hI
          · exact takeHeld_inv hI hh
      | cons q t' =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact parkQueued_inv now hI hK hq
          · exact takeHeld_inv hI hh

theorem finishW_inv (now : Int) (a : Nat) (r : RunResult) {all : List Job} {st : WS}
    (hI : InvW all st) (hK : KeysW st) : InvW all (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hI
  | cons j t =>
    simp only
    cases r with
    | ok => exact settleRun_inv now _ _ hI hK hj (Or.inl ⟨rfl, rfl⟩)
    | err e =>
      simp only
      split
      · exact holdRun_inv now hI hK hj
      · exact settleRun_inv now _ _ hI hK hj (Or.inr ⟨rfl, rfl⟩)

theorem stepW_admitted (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    ∃ l, (stepW c sz now st ev).admitted = st.admitted ++ l := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact ⟨[j], rfl⟩
    · split <;> exact ⟨[], by simp⟩
  | start a =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold startW
    split
    · rfl
    · split
      · split
        · simp [refuseHeld]
        · split
          · simp [parkQueued]
          · simp [takeHeld]
      · split
        · simp [refuseHeld]
        · split
          · rfl
          · simp [takeHeld]
      · split
        · simp [refuseQueued]
        · simp [takeQueued]
      · rfl
  | finish a r =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold finishW
    split
    · rfl
    · split
      · simp [settleRun]
      · split
        · simp [holdRun]
        · simp [settleRun]
  | wake a => exact ⟨[], by simp [stepW]⟩
  | fence => exact ⟨[], by simp [stepW]⟩

theorem runW_admitted (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), ∃ l, (runW c sz now st evs).admitted = st.admitted ++ l := by
  intro evs
  induction evs with
  | nil => intro st; exact ⟨[], by simp [runW]⟩
  | cons e es ih =>
    intro st
    obtain ⟨l1, h1⟩ := stepW_admitted c sz now st e
    obtain ⟨l2, h2⟩ := ih (stepW c sz now st e)
    exact ⟨l1 ++ l2, by simp only [runW]; rw [h2, h1, List.append_assoc]⟩

theorem stepW_inv (c : Caps) (sz : Job → Nat) (now : Int) {all : List Job} (st : WS) (ev : Ev)
    (hO : OrdW st) (hI : InvW all st)
    (hnd : ((stepW c sz now st ev).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (stepW c sz now st ev).admitted, j ∈ all) :
    InvW all (stepW c sz now st ev) := by
  obtain ⟨l, hl⟩ := stepW_admitted c sz now st ev
  have hnd0 : (st.admitted.map (·.key)).Nodup := by
    rw [hl, List.map_append] at hnd; exact (List.nodup_append.mp hnd).1
  have hK := keysW_of st hO hnd0
  cases ev with
  | submit j =>
    simp only [stepW] at hnd hall ⊢
    split
    · next hfit =>
      rw [ite_eq_left hfit] at hnd hall
      have hjn : j.key ∉ st.admitted.map (·.key) := by
        simp only [List.map_append, List.map_cons, List.map_nil] at hnd
        exact fun hm => (List.nodup_append.mp hnd).2.2 _ hm _ List.mem_cons_self rfl
      have hja : j ∈ all := hall j (List.mem_append_right _ List.mem_cons_self)
      refine ⟨hI.settled, ?_, hI.held, ?_⟩
      · intro y hy
        simp only [List.mem_append, List.mem_singleton] at hy
        rcases hy with hy | hy | hy
        · exact hI.live y (List.mem_append_left _ hy)
        · exact hI.live y (List.mem_append_right _ hy)
        · subst hy
          obtain ⟨h1, h2⟩ := hI.fresh y hja hjn
          exact ⟨⟨_, h1, rfl⟩, h2⟩
      · intro y hy hn
        exact hI.fresh y hy (fun hm => hn (by
          rw [List.map_append]; exact List.mem_append_left _ hm))
    · split
      · exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
      · exact hI
  | start a => exact startW_inv now a hI hK
  | finish a r => exact finishW_inv now a r hI hK
  | wake a => exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
  | fence =>
    refine ⟨?_, hI.live, hI.held, hI.fresh⟩
    intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    refine ⟨h1, ?_⟩
    rcases h2 with h | h | ⟨e1, _, e3⟩
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
    · exact Or.inr (Or.inr ⟨e1, rfl, e3⟩)

theorem runW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) :
    ∀ (evs : List Ev) (st : WS), OrdW st → InvW all st →
      ((runW c sz now st evs).admitted.map (·.key)).Nodup →
      (∀ j ∈ (runW c sz now st evs).admitted, j ∈ all) → InvW all (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st _ hI _ _; exact hI
  | cons e es ih =>
    intro st hO hI hnd hall
    obtain ⟨l, hl⟩ := runW_admitted c sz now es (stepW c sz now st e)
    have hnd' : (runW c sz now (stepW c sz now st e) es).admitted.map (·.key) =
        (stepW c sz now st e).admitted.map (·.key) ++ l.map (·.key) := by
      rw [hl, List.map_append]
    have hnd1 : ((stepW c sz now st e).admitted.map (·.key)).Nodup := by
      have := hnd; simp only [runW] at this; rw [hnd'] at this
      exact (List.nodup_append.mp this).1
    have hall1 : ∀ j ∈ (stepW c sz now st e).admitted, j ∈ all := by
      intro j hj; apply hall; simp only [runW]; rw [hl]; exact List.mem_append_left _ hj
    exact ih _ (stepW_ord c sz now st e hO) (stepW_inv c sz now st e hO hI hnd1 hall1) hnd hall

/-- The current workers' reachable states keep `InvW`. -/
theorem sessionW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev)
    (hnd : ((runW c sz now (WS.init (admitStore all)) evs).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (runW c sz now (WS.init (admitStore all)) evs).admitted, j ∈ all) :
    InvW all (runW c sz now (WS.init (admitStore all)) evs) :=
  runW_inv c sz now all evs _ (ordW_init _) (invW_init all) hnd hall

/-! ### The close -/

theorem ansOf_closeW_settled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {x : Answer}
    (h : ansOf st.store k = some x) (hs : x.isSettled = true) (hc : x ≠ .closeUnflushed) :
    ansOf (closeW flush now st) k = some x := by
  unfold ansOf closeW at *
  rw [lookup_settleDeferred]
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = x := by simpa using h
    split
    · simp [deferEntry_of_settled flush now e (he ▸ hs) (he ▸ hc), he]
    · simp [he]

/-- What the close answers for a write it set aside. -/
def closeAnswer : CloseFlush → Answer
  | .durable => .intentRecorded
  | .incomplete => .closeUnflushed

theorem ansOf_closeW_unsettled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {a : Answer}
    (h : ansOf st.store k = some a) (hu : a.isSettled = false) :
    k ∈ deferredW st ∧ ansOf (closeW flush now st) k = some (closeAnswer flush) := by
  unfold ansOf at h
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = a := by simpa using h
    have hu' : e.answer.isSettled = false := he ▸ hu
    have hm : k ∈ deferredW st := mem_pendingIds (lookup_mem _ _ _ hl) hu'
    refine ⟨hm, ?_⟩
    unfold ansOf closeW
    rw [lookup_settleDeferred, ite_eq_left hm, hl]
    have hs : ¬ e.answer.isSettled = true := by rw [hu']; decide
    cases flush with
    | durable =>
      have hc : e.answer ≠ .closeUnflushed := by
        intro hc; rw [hc] at hu'; exact absurd hu' (by decide)
      simp only [Option.map_some, deferEntry_durable, ite_eq_right hc,
        Lambo.Receipts.settle_unsettled e _ now hs]
      rfl
    | incomplete =>
      simp only [Option.map_some, deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now hs]
      rfl

theorem owedW_of {all : List Job} {st : WS} {j : Job} (hj : j ∈ all) (hu : Untouched j.key st.log) :
    OwedW all st j := by
  apply List.mem_filter.mpr
  refine ⟨(fixedLoad_perm _).mem_iff.mpr ?_, rfl⟩
  unfold intentsOf
  rw [List.flatten_append, List.foldl_append, ← intentsOf, intentsOf_admit]
  exact foldl_keeps_owed _ _ (List.mem_map.mpr ⟨j, hj, rfl⟩) hu

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
theorem shutdown_no_silent_lossW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (now' : Int) (j : Job) (hj : j ∈ st.admitted) :
    ((j, Answer.applied) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .failed ∧
      [Mut.consume j.key .failed] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧
      ansOf (closeW flush now' st) j.key = some .intentRecorded ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .incomplete ∧
      ansOf (closeW flush now' st) j.key = some .closeUnflushed) := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hja := hall j hj
  have hD := mem_DW_of_admitted hO hj
  simp only [DW, List.mem_append] at hD
  rcases hD with ((hD | hD) | hD) | hD
  · obtain ⟨p, hp, hpj⟩ := List.mem_map.mp hD
    subst hpj
    obtain ⟨h1, h2⟩ := hI.settled p hp
    rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
    · refine Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩)
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inr (Or.inl ⟨by rw [← e1]; exact hp, e2, ?_, owedW_of hja e3⟩))
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨h1, h3⟩ := hI.held j hD
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 rfl).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_right _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))


/-- **A held write at close.** A write held in session (an outage, a run of
lookup timeouts, or behind an earlier held write of its agent) answers
`pending_replay` while held, and its intent is unconsumed; the close never
answers it `failed`: `intent_durable` after a completed final flush (and it
stays owed for the next serve's replay), `close_unflushed` otherwise. A held
write can still settle `failed` in session once it leaves `held`: its retry
meets a content refusal, or it is refused after the fence. -/
theorem held_at_closeW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev}
    {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (j : Job) (hj : j ∈ st.held) (flush : CloseFlush) (now' : Int) :
    ansOf st.store j.key = some .pendingReplay ∧ OwedW all st j ∧
      ansOf (closeW flush now' st) j.key = some (closeAnswer flush) := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨h1, h2⟩ := hI.held j hj
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hj]))
  exact ⟨h1, owedW_of hja h2, (ansOf_closeW_unsettled h1 rfl).2⟩

/-- **An in-session outage holds the write.** The running write of agent `a`
that meets an embedder outage becomes a held write answering `pending_replay`,
and appends nothing (its intent is not consumed). While it stays held, `held_at_closeW` says the same of every later state and of the close.
The current counterpart of `outage_failed_but_owed`, which states the worker
before #295, where the same write answered `failed`. Rust test
`a_write_reached_during_an_in_session_embedder_outage_is_not_consumed`. -/
theorem outage_holdsW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job} {evs : List Ev}
    {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t) :
    j ∈ (stepW c sz now st (.finish a (.err .unavailable))).held ∧
      ansOf (stepW c sz now st (.finish a (.err .unavailable))).store j.key = some .pendingReplay ∧
      (stepW c sz now st (.finish a (.err .unavailable))).log = st.log ∧
      OwedW all (stepW c sz now st (.finish a (.err .unavailable))) j := by
  have hfin : stepW c sz now st (.finish a (.err .unavailable)) = holdRun now a j st := by
    simp [stepW, finishW, hrun, holdsS]
  rw [hfin]
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hm := head_mem hrun
  obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hm)
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hm]))
  exact ⟨List.mem_cons_self, ansOf_settleOne_self _ now h1 h2, rfl, owedW_of hja h3⟩

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
theorem heldErrW_holds (retried : Bool) (streak : Nat) (e : Err) :
    holdsS (heldErrW retried streak e) = true ↔
      (e = .unavailable ∨ (e = .lookupTimeout ∧ retried = false ∧ lookupTimeoutLimit ≤ streak + 1)) := by
  cases e <;> simp only [heldErrW] <;> (try split) <;> simp_all [holdsS] <;> decide

/-- **A single lookup timeout settles `failed` and consumes the intent**
(below the run limit, or the retry of a held write): the write is settled
`failed` and its batch is the consume. Rust test
`a_lookup_timeout_in_session_settles_failed_and_holds_nothing`. -/
theorem lookupTimeout_settlesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t)
    (retried : Bool) (streak : Nat) (hone : retried = true ∨ streak + 1 < lookupTimeoutLimit) :
    let st' := stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout)))
    ansOf st'.store j.key = some .failed ∧ (j, Answer.failed) ∈ st'.settled ∧
      st'.log = st.log ++ [[Mut.consume j.key .failed]] := by
  have he : heldErrW retried streak .lookupTimeout = .lookupTimeout := by
    simp only [heldErrW]
    rw [ite_eq_right]
    rintro ⟨h1, h2⟩
    rcases hone with h | h
    · rw [h] at h1; cases h1
    · omega
  have hfin : stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout))) =
      settleRun now a j .failed [.consume j.key .failed] st := by
    rw [he]; simp [stepW, finishW, hrun, holdsS]
  simp only [hfin]
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨⟨x, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ (head_mem hrun))
  exact ⟨ansOf_settleOne_self _ now h1 h2, List.mem_append_right _ (List.mem_singleton.mpr rfl), rfl⟩

/-- **A run of lookup timeouts holds the write**: a first-attempt lookup
timeout once the run has reached the limit is held, as an outage
does (`outage_holdsW`). Rust test
`a_lookup_timeout_run_holds_the_third_write_and_its_retry_settles_it`. -/
theorem lookupTimeout_run_holdsW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t)
    (streak : Nat) (hrun3 : lookupTimeoutLimit ≤ streak + 1) :
    let st' := stepW c sz now st (.finish a (.err (heldErrW false streak .lookupTimeout)))
    j ∈ st'.held ∧ ansOf st'.store j.key = some .pendingReplay ∧ st'.log = st.log ∧
      OwedW all st' j := by
  have he : heldErrW false streak .lookupTimeout = .unavailable := by
    simp [heldErrW, hrun3]
  simp only [he]
  exact outage_holdsW hst hnd hall a j t hrun

/-- **A retried close upgrades `close_unflushed`.** A write the close set aside
(running, held or queued) answers `close_unflushed` after an incomplete flush;
a retried close whose flush completes settles the same set-aside ids
(`close_deferred`, kept after an incomplete flush) and answers it
`intent_durable`. `retry_upgrades` is the same fact for one entry. -/
theorem closeW_retry_upgradesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (j : Job) (hj : j ∈ st.running ++ st.held ++ st.queue) (now1 now2 : Int) :
    ansOf (closeW .incomplete now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferred .durable now2 (deferredW st) (closeW .incomplete now1 st)) j.key =
        some .intentRecorded := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  have hu : ∃ a, ansOf (runW c sz now (WS.init (admitStore all)) evs).store j.key = some a ∧
      a.isSettled = false := by
    simp only [List.mem_append] at hj
    rcases hj with (hj | hj) | hj
    · exact (hI.live j (List.mem_append_left _ hj)).1
    · exact ⟨_, (hI.held j hj).1, rfl⟩
    · exact (hI.live j (List.mem_append_right _ hj)).1
  obtain ⟨a, h1, h2⟩ := hu
  obtain ⟨hm, hc⟩ := ansOf_closeW_unsettled (flush := .incomplete) (now := now1) h1 h2
  refine ⟨hc, ?_⟩
  unfold ansOf at h1 ⊢
  cases hl : lookup (runW c sz now (WS.init (admitStore all)) evs).store j.key with
  | none => rw [hl] at h1; simp at h1
  | some e =>
    rw [hl] at h1
    have he : e.answer.isSettled = false := by
      have : e.answer = a := by simpa using h1
      rw [this]; exact h2
    unfold closeW
    rw [lookup_settleDeferred, ite_eq_left hm, lookup_settleDeferred, ite_eq_left hm, hl]
    simp only [Option.map_some]
    rw [retry_upgrades now1 now2 e he]

/-! ### Crash and the serve that follows -/

theorem startW_log (now : Int) (a : Nat) (st : WS) : (startW now a st).log = st.log := by
  unfold startW
  split
  · rfl
  · split
    · split
      · rfl
      · split <;> rfl
    · split
      · rfl
      · split <;> rfl
    · split <;> rfl
    · rfl

theorem stepW_log (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    (stepW c sz now st ev).log = st.log ∨
      ∃ j r, (stepW c sz now st ev).log = st.log ++ [(workerStep false j r).2] := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact Or.inl rfl
    · split <;> exact Or.inl rfl
  | start a => exact Or.inl (startW_log now a st)
  | finish a r =>
    simp only [stepW]
    unfold finishW
    split
    · exact Or.inl rfl
    · next j _ _ =>
      cases r with
      | ok => exact Or.inr ⟨j, .ok, rfl⟩
      | err e =>
        simp only
        split
        · exact Or.inl rfl
        · exact Or.inr ⟨j, .err .embed, rfl⟩
  | wake a => exact Or.inl rfl
  | fence => exact Or.inl rfl

/-- **The refinement.** Every batch the current workers append is a batch the
old worker (`workerStep`, unfenced) appends for some write and run result: an
applied write's is `workerStep`'s success batch, a consumed failure's is its
`[consume failed]` (for a lookup timeout the old worker appended nothing; the
batch is the one it appends for a content refusal), and a hold or a fenced
refusal appends none. So `workerStep_atomic` and `workerStep_keys` hold of the
current worker's batches, which is what the crash argument needs. -/
theorem runW_log_refines (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), (∀ b ∈ st.log, ∃ j r, b = (workerStep false j r).2) →
      ∀ b ∈ (runW c sz now st evs).log, ∃ j r, b = (workerStep false j r).2 := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih =>
    intro st h
    apply ih
    intro b hb
    rcases stepW_log c sz now st e with hl | ⟨j, r, hl⟩
    · rw [hl] at hb; exact h b hb
    · rw [hl] at hb
      cases List.mem_append.mp hb with
      | inl hb => exact h b hb
      | inr hb => exact ⟨j, r, List.mem_singleton.mp hb⟩

/-- A row still unconsumed after a run of consumes was there, unconsumed,
before it. -/
theorem foldl_pending_mem {i' : Intent} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    i' ∈ ms.foldl applyMut acc → i'.outcome = none → i' ∈ acc := by
  intro ms
  induction ms with
  | nil => intro acc _ h _; exact h
  | cons m ms ih =>
    intro acc hnp h ho
    have h1 := ih (applyMut acc m) (fun x hx => hnp x (List.mem_cons_of_mem _ hx)) h ho
    cases m with
    | put k => exact absurd rfl (hnp _ List.mem_cons_self k)
    | write _ => exact h1
    | consume q t =>
      obtain ⟨i0, h0, he⟩ := List.mem_map.mp h1
      by_cases hq : i0.job.key = q
      · rw [ite_eq_left hq] at he; rw [← he] at ho; simp at ho
      · rw [ite_eq_right hq] at he; rw [← he]; exact h0

/-- **The current serve applies only stored, unconsumed rows.** Every write
`serveR` (both passes, the block reason after a halt) applies is a row it was
given, unconsumed: the first pass by `replayD_facts`, the retry pass by
`loopD_facts` over the rows the first pass's consumes left unconsumed
(`foldl_pending_mem`). The current counterpart of `replay_applied_pending`. -/
theorem serveR_applied_pending {live : Bool} {rout rout' : Job → RR} {s' : Streaks}
    {intents : List Intent} {x : Job} (hx : x ∈ (serveR live rout rout' s' intents).applied) :
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none := by
  rw [(serveR_log live rout rout' s' intents).2, (serveQ_log live rout rout' s' intents).2] at hx
  unfold replayP at hx
  dsimp only at hx
  split at hx
  · exact replayD_applied_pending hx
  · rcases List.mem_append.mp hx with h | h
    · exact replayD_applied_pending h
    · have hs := (loopD_facts stepQ rout' _ s' []).1.subset h
      obtain ⟨i', hi', rfl⟩ := List.mem_map.mp hs
      obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi'
      have hio : i'.outcome = none := Option.isNone_iff_eq_none.mp hi2
      have hnp := (replayD_facts stepQ live rout intents).2.2.1
      exact ⟨i', foldl_pending_mem _ _ hnp hi1 hio, rfl, hio⟩

/-- **No write is applied twice across a crash, over the current worker and
serve.** A flush makes some prefix of the session's log durable (admission's
puts, then the current workers' batches), and the process may die after any
flush. If a write is in the durable prefix, its consume is in the same batch
(`runW_log_refines`, `workerStep_atomic`), so the next serve (`serveR` over
the current load order, both passes) never applies it again. The current
counterpart of `crash_never_reapplies`; like it, about one process's log over
an empty intent table. -/
theorem crash_never_reappliesW (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore)
    (evs : List Ev) (all : List Job) (n : Nat) (r : Key) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks)
    (hw : Mut.write r ∈ ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten) :
    ∀ x ∈ (serveR live rout rout' s' (fixedLoad (intentsOf
      ((admitLog all ++ (runW c sz now (WS.init s) evs).log).take n).flatten))).applied,
      x.key ≠ r := by
  intro x hx hxr
  obtain ⟨i, hi, hix, hio⟩ := serveR_applied_pending hx
  rw [(fixedLoad_perm _).mem_iff] at hi
  have hL := runW_log_refines c sz now evs (WS.init s) (by intro b hb; simp [WS.init] at hb)
  generalize (runW c sz now (WS.init s) evs).log = L at hw hi hL
  rw [List.take_append, List.flatten_append] at hw hi
  have hwB : Mut.write r ∈ (L.take (n - (admitLog all).length)).flatten := by
    cases List.mem_append.mp hw with
    | inl h =>
      obtain ⟨j, hj⟩ := mem_admit_flatten h
      cases hj
    | inr h => exact h
  have hcB : Mut.consume r .applied ∈ (L.take (n - (admitLog all).length)).flatten := by
    obtain ⟨b, hb, hwb⟩ := List.mem_flatten.mp hwB
    obtain ⟨j, rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    exact List.mem_flatten.mpr ⟨_, hb, workerStep_atomic hwb⟩
  have hnp : NoPut (L.take (n - (admitLog all).length)).flatten := by
    intro m hm j he
    obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
    obtain ⟨j', rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he
  unfold intentsOf at hi
  rw [List.foldl_append] at hi
  exact foldl_consumed _ _ hnp (Or.inl ⟨_, hcB⟩) i hi (by rw [hix]; exact hxr) hio

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

/-- The answer the close gives in each case. -/
def FateW.answer : FateW → Answer
  | .applied => .applied
  | .failedConsumed => .failed
  | .failedFenced => .failed
  | .durable => .intentRecorded
  | .unflushed => .closeUnflushed

theorem FateW.holds_answer {all : List Job} {st : WS} {flush : CloseFlush} {now' : Int}
    {j : Job} : ∀ f : FateW, f.holds all st flush now' j →
      ansOf (closeW flush now' st) j.key = some f.answer
  | .applied, h => h.2.1
  | .failedConsumed, h => h.2.1
  | .failedFenced, h => h.2.2.1
  | .durable, h => h.2.2.1
  | .unflushed, h => h.2.2

/-- A write whose `[consume failed]` batch is in the log is not owed: the
consume comes after admission's puts, so every row of its id is consumed. -/
theorem not_owed_of_consumed {all : List Job} {st : WS} {j : Job} {t : Tag}
    (hnp : NoPut st.log.flatten) (hc : [Mut.consume j.key t] ∈ st.log) : ¬ OwedW all st j := by
  intro ho
  have hm := (List.mem_filter.mp ho).1
  rw [(fixedLoad_perm _).mem_iff] at hm
  unfold intentsOf at hm
  rw [List.flatten_append, List.foldl_append] at hm
  have hc' : Mut.consume j.key t ∈ st.log.flatten :=
    List.mem_flatten.mpr ⟨_, hc, List.mem_singleton.mpr rfl⟩
  exact foldl_consumed _ _ hnp (Or.inl ⟨t, hc'⟩) _ hm rfl rfl

/-- **Exactly one fate at close.** For every admitted write, exactly one case
of `shutdown_no_silent_lossW` holds. The cases differ by the close's answer,
except the two `failed` ones: a failure consumed in session has its
`[consume failed]` batch in the log, so its intent is not owed, while a write
refused after the fence is owed. -/
theorem shutdown_exactly_oneW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (now' : Int) (j : Job) (hj : j ∈ st.admitted) :
    (∃ f : FateW, f.holds all st flush now' j) ∧
      ∀ f g : FateW, f.holds all st flush now' j → g.holds all st flush now' j → f = g := by
  refine ⟨?_, ?_⟩
  · rcases shutdown_no_silent_lossW hst hnd hall flush now' j hj with h | h | h | h | h
    · exact ⟨.applied, h⟩
    · exact ⟨.failedConsumed, h⟩
    · exact ⟨.failedFenced, h⟩
    · exact ⟨.durable, h⟩
    · exact ⟨.unflushed, h⟩
  · have hnp : NoPut st.log.flatten := by
      subst hst
      intro m hm k he
      obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
      obtain ⟨j', rr, rfl⟩ := runW_log_refines c sz now evs (WS.init (admitStore all))
        (by intro b hb; simp [WS.init] at hb) b hb
      rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he
    intro f g hf hg
    have ha := (FateW.holds_answer f hf).symm.trans (FateW.holds_answer g hg)
    cases f <;> cases g <;> simp [FateW.answer] at ha ⊢
    · exact not_owed_of_consumed hnp hf.2.2 hg.2.2.2
    · exact not_owed_of_consumed hnp hg.2.2 hf.2.2.2

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
* **The current rule is wrong** (`closeW_vouches_dead_lettered`, a concrete
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

/-! ### The history's puts -/

theorem keysLanded_cons (b : Batch) (log : List Batch) (f : Landed) (hist : List Landed)
    (g : Landed) :
    keysLanded (b :: log) (f :: hist) g =
      (if f = g then putKeys b else []) ++ keysLanded log hist g := by
  unfold keysLanded
  by_cases h : f = g <;> simp [h]

theorem mem_putKeys {b : Batch} {j : Job} (h : Mut.put j ∈ b) : j.key ∈ putKeys b :=
  List.mem_filterMap.mpr ⟨_, h, rfl⟩

theorem keysLanded_sub : ∀ (log : List Batch) (hist : List Landed) (g : Landed) (k : Key),
    k ∈ keysLanded log hist g → k ∈ log.flatMap putKeys
  | [], _, _, _, h => by simp [keysLanded] at h
  | _ :: _, [], _, _, h => by simp [keysLanded] at h
  | b :: log, f :: hist, g, k, h => by
    rw [keysLanded_cons] at h
    rw [List.flatMap_cons]
    rcases List.mem_append.mp h with h | h
    · split at h
      · exact List.mem_append_left _ h
      · simp at h
    · exact List.mem_append_right _ (keysLanded_sub log hist g k h)

/-- With distinct put ids, no put is both committed and dead-lettered. -/
theorem keysLanded_disjoint : ∀ (log : List Batch) (hist : List Landed),
    (log.flatMap putKeys).Nodup → ∀ k, k ∈ committedPuts log hist → k ∉ droppedPuts log hist
  | [], _, _, _, h, _ => by simp [committedPuts, keysLanded] at h
  | _ :: _, [], _, _, h, _ => by simp [committedPuts, keysLanded] at h
  | b :: log, f :: hist, hnd, k, hc, hd => by
    unfold committedPuts at hc
    unfold droppedPuts at hd
    rw [List.flatMap_cons] at hnd
    obtain ⟨_, hrest, hdisj⟩ := List.nodup_append.mp hnd
    rw [keysLanded_cons] at hc hd
    rcases List.mem_append.mp hc with hc | hc <;> rcases List.mem_append.mp hd with hd | hd
    · split at hc
      · split at hd
        · rename_i h1 h2; rw [h1] at h2; cases h2
        · simp at hd
      · simp at hc
    · split at hc
      · exact hdisj _ hc _ (keysLanded_sub log hist _ k hd) rfl
      · simp at hc
    · split at hd
      · exact hdisj _ hd _ (keysLanded_sub log hist _ k hc) rfl
      · simp at hd
    · exact keysLanded_disjoint log hist hrest k hc hd

theorem landed_of_mem : ∀ (log : List Batch) (hist : List Landed), hist.length = log.length →
    ∀ b ∈ log, ∃ f, (b, f) ∈ log.zip hist
  | [], _, _, _, hb => by simp at hb
  | _ :: _, [], h, _, _ => by simp at h
  | c :: log, f :: hist, h, b, hb => by
    rcases List.mem_cons.mp hb with rfl | hb
    · exact ⟨f, by simp⟩
    · obtain ⟨g, hg⟩ := landed_of_mem log hist (by simpa using h) b hb
      exact ⟨g, by simp [hg]⟩

theorem mem_keysLanded {log : List Batch} {hist : List Landed} {b : Batch} {f : Landed} {j : Job}
    (hb : (b, f) ∈ log.zip hist) (hj : Mut.put j ∈ b) : j.key ∈ keysLanded log hist f :=
  List.mem_flatMap.mpr ⟨(b, f), List.mem_filter.mpr ⟨hb, by simp⟩, mem_putKeys hj⟩

theorem putKeys_admitLog : ∀ (all : List Job), (admitLog all).flatMap putKeys = all.map (·.key)
  | [] => rfl
  | j :: js => by
    have h : admitLog (j :: js) = [Mut.put j] :: admitLog js := rfl
    rw [h, List.flatMap_cons, putKeys_admitLog js]
    rfl

theorem putKeys_noPut (log : List Batch) (h : NoPut log.flatten) : log.flatMap putKeys = [] := by
  apply List.eq_nil_iff_forall_not_mem.mpr
  intro k hk
  obtain ⟨b, hb, hk⟩ := List.mem_flatMap.mp hk
  obtain ⟨m, hm, he⟩ := List.mem_filterMap.mp hk
  cases m with
  | put j => exact h _ (List.mem_flatten.mpr ⟨b, hb, hm⟩) j rfl
  | write _ => simp at he
  | consume _ _ => simp at he

/-- The current worker's batches carry no put. -/
theorem runW_noPut (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev) :
    NoPut (runW c sz now (WS.init (admitStore all)) evs).log.flatten := by
  intro m hm k he
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j', rr, rfl⟩ := runW_log_refines c sz now evs (WS.init (admitStore all))
    (by intro b hb; simp [WS.init] at hb) b hb
  rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he

/-- Every put of the session's log has its own receipt id. -/
theorem sessLog_puts_nodup {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hkeys : (all.map (·.key)).Nodup) : ((sessLog all st).flatMap putKeys).Nodup := by
  subst hst
  rw [sessLog, List.flatMap_append, putKeys_admitLog,
    putKeys_noPut _ (runW_noPut c sz now all evs), List.append_nil]
  exact hkeys

/-- Once every batch has a fate, an admitted job's put is committed or
dead-lettered. -/
theorem put_landed {all : List Job} {st : WS} {hist : List Landed}
    (hcov : hist.length = (sessLog all st).length) {j : Job} (hj : j ∈ all) :
    j.key ∈ committedPuts (sessLog all st) hist ∨ j.key ∈ droppedPuts (sessLog all st) hist := by
  have hb : [Mut.put j] ∈ sessLog all st :=
    List.mem_append_left _ (List.mem_map.mpr ⟨j, hj, rfl⟩)
  obtain ⟨f, hf⟩ := landed_of_mem _ hist hcov _ hb
  cases f with
  | committed => exact Or.inl (mem_keysLanded hf List.mem_cons_self)
  | deadLettered => exact Or.inr (mem_keysLanded hf List.mem_cons_self)

/-- With every batch landed and distinct put ids, a put is dead-lettered
exactly when it is not committed. -/
theorem dropped_iff_not_committed {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hkeys : (all.map (·.key)).Nodup) {hist : List Landed}
    (hcov : hist.length = (sessLog all st).length) {j : Job} (hj : j ∈ all) :
    j.key ∈ droppedPuts (sessLog all st) hist ↔ j.key ∉ committedPuts (sessLog all st) hist := by
  constructor
  · intro hd hc
    exact keysLanded_disjoint _ hist (sessLog_puts_nodup hst hkeys) _ hc hd
  · intro hc
    rcases put_landed hcov hj with h | h
    · exact absurd h hc
    · exact h

/-! ### The fixed close's receipts -/

theorem lookup_settleDeferredD (flush : CloseFlush) (dropped : List Key) (now : Int) :
    ∀ (ids : List Key) (s : RStore) (q : Key),
      lookup (settleDeferredD flush dropped now ids s) q =
        if q ∈ ids then (lookup s q).map (deferEntry (flushFor flush dropped q) now)
        else lookup s q := by
  intro ids
  induction ids with
  | nil => intro s q; simp [settleDeferredD]
  | cons r ids ih =>
    intro s q
    show lookup (settleDeferredD flush dropped now ids
      (update s r (deferEntry (flushFor flush dropped r) now))) q = _
    rw [ih, lookup_update]
    by_cases hr : q = r
    · subst hr
      by_cases hq : q ∈ ids
      · cases h : lookup s q with
        | none => simp
        | some e => simp [hq, deferEntry_idem]
      · simp [hq]
    · by_cases hq : q ∈ ids
      · simp [hq, hr]
      · simp [hq, hr]

/-- At each receipt the fixed close settles by its own `flushFor`. -/
theorem lookup_settleDeferredD_eq (flush : CloseFlush) (dropped : List Key) (now : Int)
    (ids : List Key) (s : RStore) (q : Key) :
    lookup (settleDeferredD flush dropped now ids s) q =
      lookup (settleDeferred (flushFor flush dropped q) now ids s) q := by
  rw [lookup_settleDeferredD, lookup_settleDeferred]

theorem ansOf_closeD (flush : CloseFlush) (dropped : List Key) (now : Int) (st : WS) (k : Key) :
    ansOf (closeD flush dropped now st) k = ansOf (closeW (flushFor flush dropped k) now st) k := by
  unfold ansOf closeD closeW
  rw [lookup_settleDeferredD_eq]

/-- **With no dead letter, the fixed close is the old one.** -/
theorem closeD_agrees (flush : CloseFlush) (now : Int) (st : WS) (k : Key) :
    ansOf (closeD flush [] now st) k = ansOf (closeW flush now st) k := by
  rw [ansOf_closeD]
  simp [flushFor]

theorem setAside_unsettled {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    ∃ a, ansOf st.store j.key = some a ∧ a.isSettled = false := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  simp only [List.mem_append] at hj
  rcases hj with (hj | hj) | hj
  · exact (hI.live j (List.mem_append_left _ hj)).1
  · exact ⟨_, (hI.held j hj).1, rfl⟩
  · exact (hI.live j (List.mem_append_right _ hj)).1

theorem setAside_mem_all {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) : j ∈ all := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  apply hall j
  apply (keysW_of _ hO hnd).adm j
  simp only [DW, List.mem_append] at hj ⊢
  rcases hj with (hj | hj) | hj
  · exact Or.inl (Or.inl (Or.inr hj))
  · exact Or.inl (Or.inr hj)
  · exact Or.inr hj

/-- A set-aside write's answer under the fixed close. -/
theorem closeD_setAside {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (dropped : List Key) (now' : Int)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    ansOf (closeD flush dropped now' st) j.key =
      some (closeAnswer (flushFor flush dropped j.key)) := by
  obtain ⟨a, h1, h2⟩ := setAside_unsettled hst hnd hall hj
  rw [ansOf_closeD]
  exact (ansOf_closeW_unsettled h1 h2).2

/-! ### The current rule is wrong -/

/-- **#345, in general: the current close vouches for a dead-lettered put.**
In every reachable session, a set-aside write (running, held or queued when
the close began) whose put the history dead-lettered answers
`intent_durable` at a close whose flush completed, though its put is not
committed. -/
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

/-! ### The fixed rule -/

/-- **Shutdown accounting under the fixed close (#345).** As
`shutdown_no_silent_lossW`, except that a set-aside write answers
`intent_durable` only when the flush completed and its put was not
dead-lettered, and `close_unflushed` when the flush was incomplete or its put
was dead-lettered. -/
theorem shutdown_no_silent_lossD {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (dropped : List Key) (now' : Int) (j : Job) (hj : j ∈ st.admitted) :
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
      ansOf (closeD flush dropped now' st) j.key = some .closeUnflushed) := by
  rw [ansOf_closeD]
  rcases shutdown_no_silent_lossW hst hnd hall (flushFor flush dropped j.key) now' j hj with
    h | h | h | ⟨h1, h2, h3, h4⟩ | ⟨h1, h2, h3⟩
  · exact Or.inl h
  · exact Or.inr (Or.inl h)
  · exact Or.inr (Or.inr (Or.inl h))
  · unfold flushFor at h2
    split at h2
    · cases h2
    · rename_i hn
      exact Or.inr (Or.inr (Or.inr (Or.inl ⟨h1, h2, hn, h3, h4⟩)))
  · unfold flushFor at h2
    split at h2
    · rename_i hm
      exact Or.inr (Or.inr (Or.inr (Or.inr ⟨h1, Or.inr hm, h3⟩)))
    · exact Or.inr (Or.inr (Or.inr (Or.inr ⟨h1, Or.inl h2, h3⟩)))

/-- **The fixed close never vouches for a dropped put.** An admitted write
whose put a dead letter dropped never answers `intent_durable`, whatever the
flush outcome. -/
theorem closeD_never_vouches_dropped {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (hist : List Landed) (now' : Int) (j : Job) (hj : j ∈ st.admitted)
    (hd : j.key ∈ droppedPuts (sessLog all st) hist) :
    ansOf (closeD flush (droppedPuts (sessLog all st) hist) now' st) j.key ≠
      some .intentRecorded := by
  intro hv
  rcases shutdown_no_silent_lossD hst hnd hall flush (droppedPuts (sessLog all st) hist) now' j hj
    with ⟨_, h, _⟩ | ⟨_, h, _⟩ | ⟨_, _, h, _⟩ | ⟨_, _, hn, _, _⟩ | ⟨_, _, h⟩
  · rw [hv] at h; cases h
  · rw [hv] at h; cases h
  · rw [hv] at h; cases h
  · exact hn hd
  · rw [hv] at h; cases h

/-- **The fixed close vouches only for committed puts.** Once every batch of
the log has landed (a completed or empty final flush), an admitted write
that answers `intent_durable` has its put in a committed batch. -/
theorem closeD_vouches_only_committed {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (hist : List Landed) (now' : Int) (j : Job) (hj : j ∈ st.admitted)
    (hcov : hist.length = (sessLog all st).length)
    (hv : ansOf (closeD flush (droppedPuts (sessLog all st) hist) now' st) j.key =
      some .intentRecorded) :
    j.key ∈ committedPuts (sessLog all st) hist := by
  rcases put_landed hcov (hall j hj) with h | h
  · exact h
  · exact absurd hv (closeD_never_vouches_dropped hst hnd hall flush hist now' j hj h)

/-- **The fixed rule is exact.** At a close whose flush completed (every
batch landed), a set-aside write answers `intent_durable` exactly when its
put is committed, and `close_unflushed` otherwise. -/
theorem closeD_durable_iff {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (hkeys : (all.map (·.key)).Nodup) (hist : List Landed) (now' : Int)
    (hcov : hist.length = (sessLog all st).length)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    (ansOf (closeD .durable (droppedPuts (sessLog all st) hist) now' st) j.key =
        some .intentRecorded ↔ j.key ∈ committedPuts (sessLog all st) hist) ∧
      (j.key ∉ committedPuts (sessLog all st) hist →
        ansOf (closeD .durable (droppedPuts (sessLog all st) hist) now' st) j.key =
          some .closeUnflushed) := by
  have hiff := dropped_iff_not_committed hst hkeys hcov (setAside_mem_all hst hnd hall hj)
  rw [closeD_setAside hst hnd hall .durable _ now' hj]
  unfold flushFor
  by_cases hd : j.key ∈ droppedPuts (sessLog all st) hist
  · rw [ite_eq_left hd]
    have hc := hiff.mp hd
    exact ⟨⟨fun h => by simp [closeAnswer] at h, fun h => absurd h hc⟩, fun _ => rfl⟩
  · rw [ite_eq_right hd]
    have hc : j.key ∈ committedPuts (sessLog all st) hist := by
      exact Classical.byContradiction (fun hn => hd (hiff.mpr hn))
    exact ⟨⟨fun _ => hc, fun _ => rfl⟩, fun hn => absurd hc hn⟩

/-- **A retried close keeps the rule.** A set-aside write answers
`close_unflushed` after an incomplete flush; a retried close whose flush
completes upgrades it to `intent_durable` only if its put was not
dead-lettered, and leaves it `close_unflushed` otherwise (the Rust's
upgrade in `settle_deferred` is skipped for a dead-lettered receipt). -/
theorem closeD_retryD {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (dropped : List Key) {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue)
    (now1 now2 : Int) :
    ansOf (closeD .incomplete dropped now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferredD .durable dropped now2 (deferredW st)
          (closeD .incomplete dropped now1 st)) j.key =
        some (if j.key ∈ dropped then .closeUnflushed else .intentRecorded) := by
  have hinc : ∀ k, flushFor .incomplete dropped k = .incomplete := by
    intro k; unfold flushFor; split <;> rfl
  obtain ⟨a, h1, h2⟩ := setAside_unsettled hst hnd hall hj
  have hm := (ansOf_closeW_unsettled (flush := .incomplete) (now := now1) h1 h2).1
  have hc1 : ansOf (closeD .incomplete dropped now1 st) j.key = some .closeUnflushed := by
    rw [ansOf_closeD, hinc]; exact (ansOf_closeW_unsettled h1 h2).2
  refine ⟨hc1, ?_⟩
  have hl : lookup (closeD .incomplete dropped now1 st) j.key =
      lookup (closeW .incomplete now1 st) j.key := by
    unfold closeD closeW; rw [lookup_settleDeferredD_eq, hinc]
  unfold ansOf
  rw [lookup_settleDeferredD, ite_eq_left hm, hl]
  unfold flushFor
  by_cases hd : j.key ∈ dropped
  · rw [ite_eq_left hd, ite_eq_left hd]
    unfold ansOf at hc1
    rw [hl] at hc1
    cases hx : lookup (closeW .incomplete now1 st) j.key with
    | none => rw [hx] at hc1; cases hc1
    | some x =>
      rw [hx] at hc1
      have hxa : x.answer = .closeUnflushed := by simpa using hc1
      have hs : x.answer.isSettled = true := by rw [hxa]; rfl
      simp only [Option.map_some, deferEntry_incomplete,
        Lambo.Receipts.settle_settled x _ now2 hs, hxa]
  · rw [ite_eq_right hd, ite_eq_right hd]
    have hr := (closeW_retry_upgradesW hst hnd hall j hj now1 now2).2
    unfold ansOf at hr
    rw [lookup_settleDeferred, ite_eq_left hm] at hr
    exact hr

/-! ### What the Rust keeps of the dead-lettered puts

`keptDropped` is the set a first version of the #345 review fix kept
(`1b4d89e4`): `Graph::consume_write_intent` forgot a receipt once its
write's consume was appended, so the set held the dropped puts whose writes
the log had not consumed. In this model a set-aside write's intent is never
consumed (`InvW`), so forgetting the consumed ones changes no answer
(`closeD_keptDropped`). **That premise is the model's, not the Rust's:** a
lane worker that panics after its commit and before its settle leaves a
consumed write unsettled, and the close sets it aside. `stepW` has no
panic, so `closeD_keptDropped` does not see it; the section "Worker panics
and the dead-lettered set" models it, shows the consume-time pruning
vouching for a dropped put there (`keptAtConsume_vouches_after_panic`), and
proves the current Rust's rule, pruning at the settle
(`settlePrune_never_vouches_dropped`). -/

/-- Whether the log consumes receipt `k`. -/
def consumedIn (log : List Batch) (k : Key) : Bool :=
  log.flatten.any (fun m => match m with
    | .consume k' _ => decide (k' = k)
    | _ => false)

/-- The dead-lettered puts the Rust still holds at close
(`Graph::dead_lettered_intents`): those whose write the log has not
consumed. -/
def keptDropped (log : List Batch) (dropped : List Key) : List Key :=
  dropped.filter (fun k => !consumedIn log k)

theorem consumedIn_false {log : List Batch} {k : Key} (h : Untouched k log) :
    consumedIn log k = false := by
  unfold consumedIn
  rw [List.any_eq_false]
  intro m hm
  cases m with
  | put _ => simp
  | write _ => simp
  | consume k' t =>
    simp only [decide_eq_true_eq]
    intro hk
    subst hk
    exact h t hm

theorem setAside_untouched {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) : Untouched j.key st.log := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  simp only [List.mem_append] at hj
  rcases hj with (hj | hj) | hj
  · exact (hI.live j (List.mem_append_left _ hj)).2
  · exact (hI.held j hj).2
  · exact (hI.live j (List.mem_append_right _ hj)).2

/-- **Forgetting consumed receipts changes no set-aside write's answer**, in
this model: the close over the kept set answers every set-aside write as the
close over all the dead-lettered puts does. It rests on `InvW` (a set-aside
write's intent is unconsumed), which a lane worker that panics after its
commit breaks in the Rust; `stepW` has no panic. With one, consume-time
pruning vouches for a dropped put (`keptAtConsume_vouches_after_panic`); the
current Rust prunes at the settle (`settlePrune_never_vouches_dropped`). -/
theorem closeD_keptDropped {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (dropped : List Key) (now' : Int)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    ansOf (closeD flush (keptDropped st.log dropped) now' st) j.key =
      ansOf (closeD flush dropped now' st) j.key := by
  rw [closeD_setAside hst hnd hall flush _ now' hj, closeD_setAside hst hnd hall flush _ now' hj]
  have hc := consumedIn_false (setAside_untouched hst hnd hall hj)
  have hm : j.key ∈ keptDropped st.log dropped ↔ j.key ∈ dropped := by
    unfold keptDropped
    simp [hc]
  unfold flushFor
  by_cases hd : j.key ∈ dropped
  · rw [ite_eq_left (hm.mpr hd), ite_eq_left hd]
  · rw [ite_eq_right (fun h => hd (hm.mp h)), ite_eq_right hd]
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
theorem keptAtConsume_vouches_after_panic :
    let t1 := [DEv.deadLetter [kP], .consume kP]
    let t2 := [DEv.consume kP, .deadLetter [kP]]
    (t1.foldl stepK DS.init).dropped = [kP] ∧ vouches (t1.foldl stepK DS.init) kP = true ∧
      (t2.foldl stepK DS.init).dropped = [kP] ∧ vouches (t2.foldl stepK DS.init) kP = true ∧
      vouches (t1.foldl stepP DS.init) kP = false ∧ vouches (t2.foldl stepP DS.init) kP = false := by
  decide

/-- The current rule's invariant. -/
structure PInv (s : DS) : Prop where
  /-- A dropped put whose write is not settled is in the set. -/
  sound : ∀ k ∈ s.dropped, k ∉ s.settled → k ∈ s.dl
  /-- The set holds only dropped puts. -/
  sub : ∀ k ∈ s.dl, k ∈ s.dropped
  /-- A consumed write not yet settled is in the record. -/
  cu_of : ∀ k ∈ s.consumed, k ∉ s.settled → k ∈ s.cu
  /-- The record holds only consumed, unsettled writes. -/
  cu_sub : ∀ k ∈ s.cu, k ∈ s.consumed ∧ k ∉ s.settled

theorem pinv_init : PInv DS.init := by
  refine ⟨?_, ?_, ?_, ?_⟩ <;> intro k hk <;> simp [DS.init] at hk

/-- The step a worker can take: it never consumes a write it already
settled (a write is consumed at most once, before its settle). -/
def DEv.ok (s : DS) : DEv → Prop
  | .consume k => k ∉ s.settled
  | _ => True

theorem stepP_inv (s : DS) (e : DEv) (hI : PInv s) (he : e.ok s) : PInv (stepP s e) := by
  cases e with
  | consume k =>
    simp only [DEv.ok] at he
    refine ⟨hI.sound, hI.sub, ?_, ?_⟩
    · intro x hx hs
      simp only [stepP, List.mem_cons] at hx hs ⊢
      rcases hx with rfl | hx
      · exact Or.inl rfl
      · exact Or.inr (hI.cu_of x hx hs)
    · intro x hx
      simp only [stepP, List.mem_cons] at hx ⊢
      rcases hx with rfl | hx
      · exact ⟨Or.inl rfl, he⟩
      · exact ⟨Or.inr (hI.cu_sub x hx).1, (hI.cu_sub x hx).2⟩
  | settle k =>
    refine ⟨?_, ?_, ?_, ?_⟩
    · intro x hx hs
      simp only [stepP, List.mem_cons, not_or] at hx hs ⊢
      exact List.mem_filter.mpr ⟨hI.sound x hx hs.2, by simp [hs.1]⟩
    · intro x hx
      simp only [stepP] at hx ⊢
      exact hI.sub x (List.mem_filter.mp hx).1
    · intro x hx hs
      simp only [stepP, List.mem_cons, not_or] at hx hs ⊢
      exact List.mem_filter.mpr ⟨hI.cu_of x hx hs.2, by simp [hs.1]⟩
    · intro x hx
      simp only [stepP, List.mem_cons, not_or] at hx ⊢
      have h1 := List.mem_filter.mp hx
      have h2 := hI.cu_sub x h1.1
      refine ⟨h2.1, ?_, h2.2⟩
      intro hk; subst hk; simp at h1
  | deadLetter ks =>
    refine ⟨?_, ?_, hI.cu_of, hI.cu_sub⟩
    · intro x hx hs
      simp only [stepP, List.mem_append] at hx hs ⊢
      rcases hx with hx | hx
      · refine Or.inl (List.mem_filter.mpr ⟨hx, ?_⟩)
        by_cases hc : x ∈ s.consumed
        · simp [hI.cu_of x hc hs]
        · simp [hc]
      · exact Or.inr (hI.sound x hx hs)
    · intro x hx
      simp only [stepP, List.mem_append] at hx ⊢
      rcases hx with hx | hx
      · exact Or.inl (List.mem_filter.mp hx).1
      · exact Or.inr (hI.sub x hx)

/-- Every step of a worker trace keeps the invariant. -/
def DOk : DS → List DEv → Prop
  | _, [] => True
  | s, e :: es => e.ok s ∧ DOk (stepP s e) es

theorem runP_inv : ∀ (evs : List DEv) (s : DS), PInv s → DOk s evs → PInv (evs.foldl stepP s)
  | [], _, hI, _ => hI
  | e :: es, s, hI, ⟨he, hr⟩ => runP_inv es (stepP s e) (stepP_inv s e hI he) hr

/-- **The current rule never vouches for a dropped put, panics included.**
Over every worker trace, a write whose put was dead-lettered and that is not
settled (set aside by the close, a panicked worker's included) is in the
set, so the close answers it `close_unflushed`. -/
theorem settlePrune_never_vouches_dropped (evs : List DEv) (h : DOk DS.init evs) (k : Key)
    (hd : k ∈ (evs.foldl stepP DS.init).dropped) :
    vouches (evs.foldl stepP DS.init) k = false := by
  have hI := runP_inv evs DS.init pinv_init h
  unfold vouches
  by_cases hs : k ∈ (evs.foldl stepP DS.init).settled
  · simp [hs]
  · simp [hI.sound k hd hs]

/-- **The current rule is exact for the writes a close sets aside.** For a
write not settled, it is in the set exactly when its put was dropped. -/
theorem settlePrune_exact (evs : List DEv) (h : DOk DS.init evs) (k : Key)
    (hs : k ∉ (evs.foldl stepP DS.init).settled) :
    k ∈ (evs.foldl stepP DS.init).dl ↔ k ∈ (evs.foldl stepP DS.init).dropped := by
  have hI := runP_inv evs DS.init pinv_init h
  exact ⟨hI.sub k, fun hd => hI.sound k hd hs⟩

/-- **The record of consumed, unsettled writes is bounded by them**: every
entry is a write consumed and not settled (in the Rust, one per worker
between its commit and its settle, plus one per panicked worker). -/
theorem settlePrune_unsettled_bounded (evs : List DEv) (h : DOk DS.init evs) (k : Key)
    (hk : k ∈ (evs.foldl stepP DS.init).cu) :
    k ∈ (evs.foldl stepP DS.init).consumed ∧ k ∉ (evs.foldl stepP DS.init).settled :=
  (runP_inv evs DS.init pinv_init h).cu_sub k hk

/-- After a settle, a consumed write is out of the set and out of the
record, and a later dead letter of its put does not note it. -/
theorem settlePrune_settled_forgotten (s : DS) (k : Key) (hc : k ∈ s.consumed)
    (ks : List Key) :
    k ∉ (stepP s (.settle k)).dl ∧ k ∉ (stepP s (.settle k)).cu ∧
      k ∉ (stepP (stepP s (.settle k)) (.deadLetter ks)).dl := by
  have h1 : k ∉ (stepP s (.settle k)).dl := by simp [stepP]
  have h2 : k ∉ (stepP s (.settle k)).cu := by simp [stepP]
  refine ⟨h1, h2, ?_⟩
  intro hm
  simp only [stepP, List.mem_append] at hm
  rcases hm with hm | hm
  · have hk := (List.mem_filter.mp hm).2
    simp at hk
    exact hk hc
  · simp at hm

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

theorem runSince_snoc (evs : List SEv) (e : SEv) :
    runSince (evs ++ [e]) = streakNew (runSince evs) e := by
  unfold runSince
  rw [List.reverse_append]
  cases e <;> simp [streakNew]

theorem streakNew_eq_runSince_rev : ∀ (r : List SEv),
    r.reverse.foldl streakNew 0 = runSince r.reverse
  | [] => rfl
  | e :: r => by
    rw [List.reverse_cons, List.foldl_append, runSince_snoc, ← streakNew_eq_runSince_rev r]
    rfl

/-- **The fixed streak is the documented count**, over every trace. -/
theorem streakNew_eq_runSince (evs : List SEv) : evs.foldl streakNew 0 = runSince evs := by
  have h := streakNew_eq_runSince_rev evs.reverse
  rwa [List.reverse_reverse] at h

/-- **The fixed worker holds a first-attempt lookup timeout exactly when the
documented run reaches the limit.** -/
theorem heldNew_iff (evs : List SEv) :
    holdsS (heldErrW false (evs.foldl streakNew 0) .lookupTimeout) = true ↔
      lookupTimeoutLimit ≤ runSince evs + 1 := by
  rw [heldErrW_holds, streakNew_eq_runSince]
  simp

theorem foldl_streakNew_zero : ∀ (mid : List SEv), (∀ e ∈ mid, e ≠ .timeout) →
    mid.foldl streakNew 0 = 0
  | [], _ => rfl
  | e :: mid, h => by
    have he : streakNew 0 e = 0 := by
      cases e with
      | commit => rfl
      | okArm => rfl
      | settleOk => rfl
      | timeout => exact absurd rfl (h _ List.mem_cons_self)
    rw [List.foldl_cons, he]
    exact foldl_streakNew_zero mid (fun x hx => h x (List.mem_cons_of_mem _ hx))

/-- **A timeout after an applied write counts as a first timeout** under the
fixed streak, whatever came before the commit and whatever settles after it,
as long as no other timeout came since. -/
theorem streakNew_after_commit (pre mid : List SEv) (hmid : ∀ e ∈ mid, e ≠ .timeout) :
    (pre ++ SEv.commit :: mid).foldl streakNew 0 = 0 := by
  rw [List.foldl_append, List.foldl_cons]
  exact foldl_streakNew_zero mid hmid

/-- **In the worker model, that timeout settles `failed`**: the running write
of agent `a` whose first attempt times out after a commit (no timeout since)
is settled `failed` with its intent consumed, by `lookupTimeout_settlesW` at
the streak the fixed worker counts. Rust tests
`a_lookup_timeout_between_a_commit_and_its_settle_settles_failed`,
`a_lookup_timeout_between_a_commit_and_its_ok_arm_settles_failed`. -/
theorem timeout_after_commit_settlesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (a : Nat) (j : Job) (t : List Job) (hrun : ofAgent a st.running = j :: t)
    (pre mid : List SEv) (hmid : ∀ e ∈ mid, e ≠ .timeout) :
    let st' := stepW c sz now st
      (.finish a (.err (heldErrW false ((pre ++ SEv.commit :: mid).foldl streakNew 0) .lookupTimeout)))
    ansOf st'.store j.key = some .failed ∧ (j, Answer.failed) ∈ st'.settled ∧
      st'.log = st.log ++ [[Mut.consume j.key .failed]] :=
  lookupTimeout_settlesW hst hnd hall a j t hrun false _
    (Or.inr (by rw [streakNew_after_commit pre mid hmid]; decide))

/-- **#346: the streak before the fix counts a timeout that follows a
commit.** Two lookup timeouts, then another lane's write commits; before
that write's settle block resets the streak, a third first-attempt timeout
takes the lanes lock. The old streak reads 2 and holds it as the third of a
run; nothing was timed out since the commit (`runSince` is 0), so the rule
says it is a first timeout, and the fixed streak settles it `failed`. Rust
test `a_lookup_timeout_between_a_commit_and_its_settle_settles_failed`. -/
theorem streak_old_counts_across_commit :
    let pre := [SEv.timeout, .timeout, .commit]
    holdsS (heldErrW false (pre.foldl streakOld 0) .lookupTimeout) = true ∧
      runSince pre = 0 ∧
      holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = false := by
  decide

/-- **#346's first fix still counted a timeout that follows a commit.** It
reset the streak in the worker's `Ok` arm, which runs after `WriteCtx::run`
returns: after the index mirroring, which waits for the graph read and index
write locks. Two lookup timeouts, then another lane's write commits; before
its `Ok` arm runs, a third first-attempt timeout takes the lanes lock. The
`Ok`-arm streak reads 2 and holds it; `runSince` is 0, and the fixed streak,
reset inside the commit, settles it `failed`. Rust test
`a_lookup_timeout_between_a_commit_and_its_ok_arm_settles_failed`. -/
theorem streak_okArm_counts_across_commit :
    let pre := [SEv.timeout, .timeout, .commit]
    holdsS (heldErrW false (pre.foldl streakOkArm 0) .lookupTimeout) = true ∧
      runSince pre = 0 ∧
      holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = false := by
  decide

/-- **Keeping the settle's reset as well is wrong the other way.** A write
commits, two timeouts follow, then its settle runs: with both resets the
streak reads 0 and the third timeout settles `failed`, though it is the third
with nothing applied between (`runSince` is 2) and the rule holds it, as the
fixed streak does. -/
theorem streak_both_forgets_run :
    let pre := [SEv.commit, .timeout, .timeout, .settleOk]
    holdsS (heldErrW false (pre.foldl streakBoth 0) .lookupTimeout) = false ∧
      runSince pre = 2 ∧
      holdsS (heldErrW false (pre.foldl streakNew 0) .lookupTimeout) = true := by
  decide

end Lambo.Drain
