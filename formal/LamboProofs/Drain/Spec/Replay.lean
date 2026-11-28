/-
# Write-queue drain: the replay fixes (#294, #295) and the in-session hold

The replay as #294 and #295 changed it, on top of `Base.lean`'s step
(`replayStep`), and the first model of #295's in-session hold. The current
worker is in `Worker.lean`, the close in `Close.lean`.

This is the specification layer: the definitions and each result as a
proposition `Spec.<name>` (`Spec/Base.lean` describes the two layers). The
proofs are in `Drain/Proofs/Replay.lean`.

## Results

* The fix for #294 (`stepInput`, run through `loopWith`, the loop with its
  step as a parameter; `loopWith_replayStep` shows the generalisation is the
  old loop): `fixed_input_goes_on`, `stepInput_refusesInput`: the replay
  settles every input refusal `failed` and goes on, now including the
  graph-dependent refusals the Rust marks with `StoreError::input_refusal`
  (`Err.inputRefusal`); `fixed_sessionWide_stops`, `stepInput_sessionWide`:
  it still stops on every other non-transient error; `stepInput_agrees`:
  nothing else changes; `fixed_agrees_with_worker`: replay and the in-session
  worker now both consume an input refusal `failed`.
* The fix for #295 (`stepFixed` on top of `stepInput`, run through `loopD`,
  which defers an agent for the rest of the serve at a transient answer, and
  loaded by `fixedLoad`, which orders each process's rows by sequence number
  within the positions the old load gave that process):
  `loopD_no_overtake`: within a serve no write is applied while an earlier
  write of its agent is left unsettled, for any step and any run results;
  `fixed_agent_order`, `fixed_agent_order_load`: across two serves, whatever
  the embedder answers, each agent's writes of one process apply in
  admission order; `fixedLoad_sorted`, `fixed_epoch_seq_order`: per-process
  sequence order with no clock assumption; `fixedLoad_epochs`: the
  cross-process order is the old one; `fixedLoad_agrees`: while the clock
  behaves the load is unchanged; `fixed_badId_settled`: an unparseable
  receipt id is settled `failed`; `fixedD_input_goes_on`: #294 still holds.
* The in-session hold (#295; `drainHeld`, `arriveH`, `workH`): the lane
  worker keeps an agent's writes met by an embedder outage held, answers them
  `pending_replay`, and runs them, oldest first, before the agent's next
  write. `workH_split`, `hold_split`: for every agent, the writes settled in
  session followed by those still held are exactly its writes in admission
  order. `session_orderInv`, `hold_then_serve_order`: each agent's writes of
  one process apply in admission order in session and across the next serve,
  whatever the embedder answers. `arriveH_clears`: an agent whose held writes
  all get an answer other than an outage holds nothing after its next write.
  `OrderInv`, `loopD_preserves`, `replayD_preserves`: the order argument as an
  invariant each pass keeps.
* The replay after review (#295; `stepQ`, `replayP`, `serveQ`): a retry pass
  gives each deferred agent one more attempt in the serve, an id in another
  receipt format is quarantined (owed, its agent deferred) rather than
  settled, and a serve that ends with writes owed names a block reason.
  `replayP_progress`: if the retry gets an answer for every write it runs,
  nothing is owed after the serve; `replayP_preserves`,
  `hold_then_serveQ_order`: the order invariant holds through both passes,
  from the in-session hold on; `serveQ_block`: the block reason is never
  null while writes are owed; `stepQ_badId`, `stepQ_unknownFormat`,
  `stepQ_skip`: what settles and what defers. `fixed_badId_settled` describes
  the fix as first committed, which settled every unparseable id.

Boundary: as in `Base.lean`, each replay step is atomic and a run's result is
an input per write, so what the embedder answers and why is not modelled; a
deferred agent stays deferred for the rest of the serve, and the retry pass
is one more attempt, not a timer.
-/

import LamboProofs.Drain.Spec.Base
import LamboProofs.Flush.Loop

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The fix for #294: graph-dependent input refusals settle `failed`

The Rust marks the refusals `Err.inputRefusal` stands for by a message
prefix (`StoreError::input_refusal`, recognised by
`StoreError::is_input_refusal`, which a cycle refusal also satisfies), and
the replay's content arm settles every such refusal `failed`, as the in-session
worker already does. Every other non-transient error still stops the
replay. -/

/-- The fixed classification: the errors the replay settles `failed` and goes
on past. -/
def Err.refusesInput (e : Err) : Bool := e.inputFact || e == .inputRefusal

/-- The replay's match after #294: an input refusal settles `failed` and
leaves both streaks alone (decided before any embed, like a cycle refusal);
every other result is handled as before. -/
def stepInput (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .run (.err .inputRefusal) => (.failed, s)
  | rr => replayStep image rr s

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

/-- **The fixed replay settles every input refusal `failed`**: `Embed`,
`ImageIdTaken`, `Reference`, a cycle refusal, and the graph-dependent
refusals #294 names. -/
abbrev Spec.stepInput_refusesInput : Prop :=
  ∀ (img : Bool) (e : Err) (s : Streaks),
    e.refusesInput = true →
    (stepInput img (.run (.err e)) s).1 = .failed

/-- **And still stops on everything else that is not transient**: a store
error that can mean an inconsistent graph or store, `Config`, a lost lease,
anything else. -/
abbrev Spec.stepInput_sessionWide : Prop :=
  ∀ (img : Bool) (e : Err) (s : Streaks),
    e.refusesInput = false →
    e.transient = false →
    ∃ b, (stepInput img (.run (.err e)) s).1 = .stop b

/-- **The fixed replay still stops on every other non-transient error**, the
intent and every later one left unconsumed. -/
abbrev Spec.fixed_sessionWide_stops : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent) (e : Err),
    rout i.job = .run (.err e) →
    e.refusesInput = false →
    e.transient = false →
    ∃ b, loopWith stepInput rout s (i :: rest) = { ROut.empty with block := some b }

/-! ## The fix for #295: per-agent order, a clock-free load order, bad ids

Three changes on top of #294's step.

* An intent whose receipt id does not parse is settled `failed` (`stepFixed`),
  like a payload this build cannot decode (#129): it can never be applied or
  looked up by its id, and before it was skipped on every serve and counted
  owed forever (`bad_id_never_settled`).
* A transient embedder answer below the threshold still leaves its intent
  unconsumed, and now **defers that agent** for the rest of the serve
  (`loopD`): none of its later intents is run, so none can apply before the
  one left behind. Other agents' intents go on, and the threshold still counts
  transients across all of them. The alternative, stopping the replay at the
  first transient, also keeps the order, but makes one blip hold every
  agent's backlog and leaves `EMBEDDER_SICK_THRESHOLD` with nothing to count.
* Each process's intents are loaded in sequence order within its epoch
  (`fixedLoad`): the (`issued_ms`, `lane_seq`) load decides which epoch each
  position belongs to, as before, and the epoch's own intents fill its
  positions in `lane_seq` order (`regroup`). No clock assumption is needed.

In the Rust a deferred agent's intent is passed over after the loop's lease
and seal check; here it is passed over before `rout` is consulted, which can
only move where a halting replay stops, never what it settles. -/

/-- The replay's match after #295: an unparseable receipt id settles
`failed`; everything else is `stepInput` (#294). -/
def stepFixed (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .badId => (.failed, s)
  -- #295 as first fixed settled every unparseable id.
  | .unknownFormat => (.failed, s)
  | rr => stepInput image rr s

/-- The replay loop after #295. `d` holds the agents deferred for the rest of
the serve: an intent of one is passed over, unconsumed; a `skip` (a transient
answer below the threshold) defers the intent's agent. Otherwise as
`loopWith`. -/
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

/-- `spawn_replay` after #295: the same gate, the loop with no agent deferred
yet. -/
def replayD (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loopD step rout ⟨0, 0⟩ [] p else { ROut.empty with block := some .embedder }

/-! ### The loop -/

/-- **No write overtakes an earlier one of its agent within a serve.** If the
fixed loop applies an intent, every earlier intent of the same agent in the
backlog was settled in that serve (applied or failed), never left behind for
a later serve. Holds for any step and any run results, transient answers
included: a `skip` defers the agent, so nothing of it runs after. -/
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

/-- **#294 still holds in the fixed loop**: an input refusal is consumed
`failed` and the loop goes on. -/
abbrev Spec.fixedD_input_goes_on : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (d : List Nat) (i : Intent) (rest : List Intent) (e : Err),
    rout i.job = .run (.err e) →
    e.refusesInput = true →
    i.job.agent ∉ d →
    ∃ s', loopD stepFixed rout s d (i :: rest) =
      { loopD stepFixed rout s' d rest with
        answers := (i.job.key, .failed) :: (loopD stepFixed rout s' d rest).answers
        log := [.consume i.job.key .failed] :: (loopD stepFixed rout s' d rest).log
        failed := i.job :: (loopD stepFixed rout s' d rest).failed }

/-! ### The load order -/

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

/-- **The load order after #295** (`replay_order` in `replay.rs`): the
adapters' (`issued_ms`, `lane_seq`) order picks which process each position
belongs to, and each process's intents fill its positions in `lane_seq`
order. One difference from the Rust: every row here has an epoch, so a row
whose id the replay cannot parse (`badId`, `unknownFormat`, outcomes of the
run, not of the row) is regrouped with its process; `replay_order` cannot
read an epoch from such an id and leaves the row where the load put it. Such
a row is never applied, so the order theorems are unaffected. -/
def fixedLoad (l : List Intent) : List Intent := regroup (loadOrder l)

/-- **Each process's intents load in sequence order, with no clock
assumption** (`load_epoch_seq_order` needs one; `clock_step_reorders` is
what happens without it). -/
abbrev Spec.fixedLoad_sorted : Prop :=
  ∀ (l : List Intent),
    EpochSorted (fixedLoad l)

/-- **Cross-process order as before**: each position of the load belongs to
the same process as in the (`issued_ms`, `lane_seq`) order. -/
abbrev Spec.fixedLoad_epochs : Prop :=
  ∀ (l : List Intent),
    (fixedLoad l).map (·.job.epoch) = (loadOrder l).map (·.job.epoch)

/-- **Nothing changes while the clock behaves**: under `load_epoch_seq_order`'s
hypothesis the fixed load is the old one. -/
abbrev Spec.fixedLoad_agrees : Prop :=
  ∀ (l : List Intent),
    (∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) →
    fixedLoad l = loadOrder l

/-! ## Order across passes: the invariant the order proofs carry

`fixed_agent_order` compares two serves. The in-session hold and the replay's
retry pass add more passes, so the argument is stated once as an invariant
over the rows left owed and the writes applied so far, and each pass is shown
to keep it. -/

/-- The writes applied so far keep each agent's writes of one process in
sequence order, and none is later in sequence than a write of its agent and
process still owed in `rows`. -/
def OrderInv (rows : List Intent) (applied : List Job) : Prop :=
  applied.Pairwise (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) ∧
  ∀ x ∈ applied, ∀ i ∈ rows, i.outcome = none → x.agent = i.job.agent →
    x.epoch = i.job.epoch → x.rid ≤ i.job.rid

/-- **One pass of the fixed loop keeps the invariant**, for any step, any
streaks and any run results, over rows keyed by receipt id and loaded in
per-process sequence order. -/
abbrev Spec.loopD_preserves : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) (s : Streaks)
    (rows : List Intent) (A : List Job),
    (rows.map (·.job.key)).Nodup →
    EpochSorted rows →
    OrderInv rows A →
    OrderInv (applyLog rows (loopD step rout s [] (pendingOf rows)).log)
      (A ++ (loopD step rout s [] (pendingOf rows)).applied)

/-- The same for a whole serve of the fixed replay (the liveness gate
included). -/
abbrev Spec.replayD_preserves : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (rows : List Intent) (A : List Job),
    (rows.map (·.job.key)).Nodup →
    EpochSorted rows →
    OrderInv rows A →
    OrderInv (applyLog rows (replayD step live rout rows).log)
      (A ++ (replayD step live rout rows).applied)

/-! ## The replay's retry pass, quarantine and block reason (#295)

Three changes to the fixed replay, after review.

* **Retry pass** (`replayP`). An agent deferred at a transient answer used to
  wait for the next serve, so a write that answers transiently every time held
  its agent's backlog forever. Now, unless the pass stopped, the replay runs
  once more over what it left owed (the deferred agents' writes, from each
  one's oldest owed write), with the streaks it ended with: each deferred
  agent gets one more attempt in the serve. `replayP_progress` says what that
  buys; a write that never embeds still defers its agent every serve (an
  owner question).
* **Quarantine** (`stepQ`). An id in another receipt format (`unknownFormat`,
  as a newer build writes) is not settled `failed`, which cannot be undone:
  it is left owed and its agent deferred, like a transient answer, so its
  agent's later writes do not overtake it. A malformed id in this build's own
  format (`badId`) is still settled `failed`.
* **Block reason** (`serveQ`). A serve that ends with writes owed and no stop
  now says so: `embedder` if the last pass deferred an agent at a transient
  answer, `other` if only quarantined ids hold writes back. -/

/-- The replay's step after the remediation: `unknownFormat` defers its
agent (a `skip`); everything else is `stepFixed`. -/
def stepQ (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .unknownFormat => (.skip, s)
  | rr => stepFixed image rr s

/-- Only a transient answer or a quarantined id defers an agent. -/
abbrev Spec.stepQ_skip : Prop :=
  ∀ {img : Bool} {rr : RR} {s : Streaks},
    (stepQ img rr s).1 = .skip →
    rr = .run (.err .unavailable) ∨ rr = .unknownFormat

/-- One serve: the fixed replay, then, unless it stopped, one more pass over
the rows it left owed, from the streaks `s'` (the Rust carries the first
pass's; the theorems take any). -/
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

/-- The serve after the remediation, with its block reason: when it ends with
writes owed and no stop, `embedder` if the last pass deferred an agent at a
transient answer, else `other` (only quarantined ids hold writes back). -/
def serveQ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := replayP stepQ live rout rout' s' intents
  if r.block = none ∧ pendingOf (applyLog intents r.log) ≠ [] then
    let left := pendingOf (applyLog intents (replayD stepQ live rout intents).log)
    { r with
      block := some (if (skipsOf stepQ rout' s' [] left).contains (.run (.err .unavailable))
        then .embedder else .other) }
  else r

/-- **The block reason is never null while writes are owed** after a serve
that did not stop: `write_queue_replay_blocked` names why. -/
abbrev Spec.serveQ_block : Prop :=
  ∀ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent),
    pendingOf (applyLog intents (serveQ live rout rout' s' intents).log) ≠ [] →
    (serveQ live rout rout' s' intents).block ≠ none

/-- **A serve with the retry pass keeps the order invariant**: both passes
are runs of the fixed loop over per-process-sorted rows. -/
abbrev Spec.replayP_preserves : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks) (rows : List Intent) (A : List Job),
    (rows.map (·.job.key)).Nodup →
    EpochSorted rows →
    OrderInv rows A →
    OrderInv (applyLog rows (replayP step live rout rout' s' rows).log)
      (A ++ (replayP step live rout rout' s' rows).applied)

/-- **Progress from the retry pass.** If the first pass did not stop and the
retry gets an answer for every write it runs (applied or refused: the
embedder answered and no id is quarantined), the serve leaves nothing owed.
What is not guaranteed: a write whose answer is transient at both attempts,
or a quarantined id, keeps its agent's writes owed to the next serve, every
serve if it never embeds (an owner question: an attempt bound). -/
abbrev Spec.replayP_progress : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout rout' : Job → RR)
    (s' : Streaks) (rows : List Intent),
    (replayD step live rout rows).block = none →
    (∀ j s, (step j.image (rout' j) s).1 = .applied ∨ (step j.image (rout' j) s).1 = .failed) →
    pendingOf (applyLog rows (replayP step live rout rout' s' rows).log) = []

/-- #324. After a mixed batch is rejected, the confirmed failure consume
leaves the bad receipt settled and its neighbor owed. The second restart
has just one row to run. Once it commits, neither intent is owed again. -/
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
