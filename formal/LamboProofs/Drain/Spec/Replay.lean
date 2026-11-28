/-
# Write-queue drain: the replay

The replay of a session's durable intents at serve: its step (`stepQ`, over
`stepFixed`, `stepInput` and `Base.lean`'s `replayStep`), the loop that
defers an agent at a transient answer or a quarantined id (`loopD`), the
retry pass (`replayP`), the block reason (`serveQ`, reported by `Worker.lean`'s
`serveR`), and the load order (`fixedLoad`).

Rust: `src/writeq/replay.rs` (`spawn_replay`, `replay_order`);
`src/store/replay_flush.rs`; `ReceiptId` (`src/writeq/receipts.rs`);
`ReplayBlockReason` (`src/writeq/counters.rs`); the adapters' load order
(`src/store/{sqlite,pg}/session_load.rs`); `is_cycle_refusal`,
`is_input_refusal` (`src/types/mod.rs`).

Results:
* `loopD_no_overtake`: no write applies while an earlier one of its agent is left owed.
* `fixedLoad_sorted`: each process's intents load in sequence order, with no clock assumption.
* `fixedLoad_epochs`, `fixedLoad_agrees`: across processes the adapters' order is kept, and
  while the clock behaves the load is the adapters'.
* `stepQ_skip`: only a transient answer or a quarantined id defers an agent.
* `replayP_progress`: if the retry pass gets an answer for every write, nothing stays owed.
* `replay_dead_letter_two_restarts`: a rejected mixed batch settles the bad intent and
  leaves its neighbour owed once.
* `fixed_sessionWide_stops`: a non-transient error that is not an input refusal stops the replay.

Boundary: each replay step is atomic and a run's result is an input per
write. A deferred agent stays deferred for the rest of the serve, and the
retry pass is one more attempt, not a timer. A deferred agent's intent is
passed over before its run result is read, where the Rust checks the lease
and the seal first: that moves only where a halting replay stops. `fixedLoad`
regroups a row whose id does not parse with its process, where
`replay_order` leaves it in place; such a row is never applied.
`fixed_sessionWide_stops` is stated over an earlier loop
(`loopWith stepInput`) whose arms the current step keeps for those answers,
not over `stepQ`.
-/

import LamboProofs.Drain.Spec.Base
import LamboProofs.Flush.Loop

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## Input refusals -/

/-- The errors the replay settles `failed` and goes on past: a fact about the
input, or an input refusal (`StoreError::is_input_refusal`). -/
def Err.refusesInput (e : Err) : Bool := e.inputFact || e == .inputRefusal

/-- `spawn_replay`'s content arm: an input refusal settles `failed`, both
streaks untouched; every other result goes to `replayStep`. -/
def stepInput (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .run (.err .inputRefusal) => (.failed, s)
  | rr => replayStep image rr s

/-- The replay loop with its step as a parameter and no deferral: the loop
`fixed_sessionWide_stops` is stated over. -/
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

/-- **A non-transient error that is not an input refusal stops the replay**,
the intent and every later one left unconsumed. -/
abbrev Spec.fixed_sessionWide_stops : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent) (e : Err),
    rout i.job = .run (.err e) →
    e.refusesInput = false →
    e.transient = false →
    ∃ b, loopWith stepInput rout s (i :: rest) = { ROut.empty with block := some b }

/-! ## Deferring an agent

A transient answer below the threshold leaves its intent owed and defers its
agent for the rest of the serve; the threshold counts across agents. -/

/-- An unparseable receipt id settles `failed`; everything else is
`stepInput`. `stepQ` overrides the `unknownFormat` arm. -/
def stepFixed (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .badId => (.failed, s)
  | .unknownFormat => (.failed, s)
  | rr => stepInput image rr s

/-- `spawn_replay`'s loop: `d` holds the deferred agents, whose intents are
passed over unconsumed; a `skip` defers the intent's agent. -/
def loopD (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    Streaks → List Nat → List Intent → ROut
  | _, _, [] => ROut.empty
  | s, d, i :: rest =>
    if i.job.agent ∈ d then loopD step rout s d rest
    else
      match step i.job.image (rout i.job) s with
      | (.applied, s') =>
        let o := loopD step rout s' d rest
        { o with
          answers := (i.job.key, .appliedAfterRestart) :: o.answers
          log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] :: o.log
          applied := i.job :: o.applied }
      | (.failed, s') =>
        let o := loopD step rout s' d rest
        { o with
          answers := (i.job.key, .failed) :: o.answers
          log := [.consume i.job.key .failed] :: o.log
          failed := i.job :: o.failed }
      | (.skip, s') => loopD step rout s' (i.job.agent :: d) rest
      | (.stop b, _) => { ROut.empty with block := some b }

/-- `spawn_replay`'s first pass: nothing owed, nothing to do; otherwise the
liveness embed gates the loop. -/
def replayD (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loopD step rout ⟨0, 0⟩ [] p else { ROut.empty with block := some .embedder }

/-- **No write overtakes an earlier one of its agent within a serve**: when the
loop applies an intent, every earlier intent of its agent was settled in that
serve, for any step and any run results. Rust test
`a_transient_answer_at_replay_defers_the_agent_instead_of_reordering_it`. -/
abbrev Spec.loopD_no_overtake : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR),
    ∀ (pre : List Intent) (ix : Intent) (post : List Intent) (s : Streaks) (d : List Nat),
    ((pre ++ ix :: post).map (·.job)).Nodup →
    ix.job ∈ (loopD step rout s d (pre ++ ix :: post)).applied →
    ∀ i ∈ pre, i.job.agent = ix.job.agent →
      i.job ∈ (loopD step rout s d (pre ++ ix :: post)).applied ∨
        i.job ∈ (loopD step rout s d (pre ++ ix :: post)).failed

/-- Per-epoch sequence order of a list of rows. -/
abbrev EpochSorted (l : List Intent) : Prop :=
  l.Pairwise (fun a b => a.job.epoch = b.job.epoch → a.job.rid ≤ b.job.rid)

/-! ## The load order (`replay_order`) -/

/-- The intent with the smallest sequence number among `x` and those of `l`
in `x`'s epoch (`x` itself, or the first such, on a tie). -/
def minE (x : Intent) : List Intent → Intent
  | [] => x
  | y :: ys =>
    minE (if y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid then y else x) ys

/-- `l` with its first `m` replaced by `x`. -/
def swapIn (m x : Intent) : List Intent → List Intent
  | [] => []
  | y :: ys => if y = m then x :: ys else y :: swapIn m x ys

/-- Selection within epochs: each position keeps its epoch, and an epoch's
positions take its intents in sequence order. `n` is fuel (`regroup` passes
the length). -/
def regroupN : Nat → List Intent → List Intent
  | 0, l => l
  | _, [] => []
  | n + 1, x :: xs =>
    if minE x xs = x then x :: regroupN n xs
    else minE x xs :: regroupN n (swapIn (minE x xs) x xs)

def regroup (l : List Intent) : List Intent := regroupN l.length l

/-- `replay_order`: the adapters' (`issued_ms`, `lane_seq`) order picks each
position's process, and each process's intents fill its positions in
`lane_seq` order. -/
def fixedLoad (l : List Intent) : List Intent := regroup (loadOrder l)

/-- **Each process's intents load in sequence order, with no clock
assumption.** Rust test
`a_clock_step_between_two_admissions_does_not_reorder_their_replay`. -/
abbrev Spec.fixedLoad_sorted : Prop :=
  ∀ (l : List Intent),
    EpochSorted (fixedLoad l)

/-- **The adapters' order across processes is kept**: each position of the
load belongs to the same process as in the (`issued_ms`, `lane_seq`) order. -/
abbrev Spec.fixedLoad_epochs : Prop :=
  ∀ (l : List Intent),
    (fixedLoad l).map (·.job.epoch) = (loadOrder l).map (·.job.epoch)

/-- **While the clock behaves the load is the adapters'**: when no later
sequence number of a process has an earlier issue time, it is `loadOrder`. -/
abbrev Spec.fixedLoad_agrees : Prop :=
  ∀ (l : List Intent),
    (∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) →
    fixedLoad l = loadOrder l

/-! ## Quarantine, the retry pass and the block reason

An id in another receipt format (`unknownFormat`) is quarantined: left owed
with its agent deferred. -/

/-- `spawn_replay`'s step: an id of another format defers its agent (a
`skip`); everything else is `stepFixed`. -/
def stepQ (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .unknownFormat => (.skip, s)
  | rr => stepFixed image rr s

/-- Only a transient answer or a quarantined id defers an agent. -/
abbrev Spec.stepQ_skip : Prop :=
  ∀ {img : Bool} {rr : RR} {s : Streaks},
    (stepQ img rr s).1 = .skip →
    rr = .run (.err .unavailable) ∨ rr = .unknownFormat

/-- One serve: the first pass, then, unless it stopped, one retry pass over
the rows it left owed, from streaks `s'` (the Rust carries the first pass's). -/
def replayP (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks) (intents : List Intent) : ROut :=
  let r1 := replayD step live rout intents
  match r1.block with
  | some _ => r1
  | none =>
    let r2 := loopD step rout' s' [] (pendingOf (applyLog intents r1.log))
    { answers := r1.answers ++ r2.answers
      log := r1.log ++ r2.log
      applied := r1.applied ++ r2.applied
      failed := r1.failed ++ r2.failed
      block := r2.block }

/-- What the loop met at each intent it deferred an agent at, in order. -/
def skipsOf (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    Streaks → List Nat → List Intent → List RR
  | _, _, [] => []
  | s, d, i :: rest =>
    if i.job.agent ∈ d then skipsOf step rout s d rest
    else
      match step i.job.image (rout i.job) s with
      | (.applied, s') => skipsOf step rout s' d rest
      | (.failed, s') => skipsOf step rout s' d rest
      | (.skip, s') => rout i.job :: skipsOf step rout s' (i.job.agent :: d) rest
      | (.stop _, _) => []

/-- The serve with its block reason: ended with writes owed and no stop, it is
`embedder` if the retry pass deferred an agent at a transient, else `other`. -/
def serveQ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := replayP stepQ live rout rout' s' intents
  if r.block = none ∧ pendingOf (applyLog intents r.log) ≠ [] then
    let left := pendingOf (applyLog intents (replayD stepQ live rout intents).log)
    { r with
      block := some (if (skipsOf stepQ rout' s' [] left).contains (.run (.err .unavailable))
        then .embedder else .other) }
  else r

/-- **The retry pass leaves nothing owed when it gets an answer for every
write** (applied or refused), after a first pass that did not stop. Rust test
`a_deferred_agent_is_retried_once_at_the_end_of_the_pass`. -/
abbrev Spec.replayP_progress : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks) (rows : List Intent),
    (replayD step live rout rows).block = none →
    (∀ j s, (step j.image (rout' j) s).1 = .applied ∨ (step j.image (rout' j) s).1 = .failed) →
    pendingOf (applyLog rows (replayP step live rout rout' s' rows).log) = []

/-- **A rejected mixed batch settles the bad intent and leaves its neighbour
owed once**: after the second restart's commit neither is owed. Rust test
`a_dead_letter_does_not_replay_the_bad_intent_on_every_restart`. -/
abbrev Spec.replay_dead_letter_two_restarts : Prop :=
  ∀ (bad good : Job),
    bad.key ≠ good.key →
    let rows : List Intent := [⟨bad, none⟩, ⟨good, none⟩]
    let goodSpan : Batch := [.write good.key, .consume good.key .appliedAfterRestart]
    let first := applyLog rows (Flush.replayRecoveryLog
      [.settledRejection [.consume bad.key .failed], .committed goodSpan])
    pendingOf first = [⟨good, none⟩] ∧
      applyLog first [goodSpan] = [⟨bad, some .failed⟩, ⟨good, some .appliedAfterRestart⟩]

end Lambo.Drain
