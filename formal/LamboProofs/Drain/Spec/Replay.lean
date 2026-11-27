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

/-- `replay` with its step as a parameter. -/
def replayWith (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loopWith step rout ⟨0, 0⟩ p else { ROut.empty with block := some .embedder }

/-- The generalisation agrees with the loop as it was. -/
abbrev Spec.loopWith_replayStep : Prop :=
  ∀ (rout : Job → RR),
    ∀ (l : List Intent) (s : Streaks),
      loopWith replayStep rout s l = loop rout s l

/-- The replay loop with its step as a parameter (`loopWith`) is the replay
before #294 when given `replayStep`: #294 changed the step, not the loop. -/
abbrev Spec.replayWith_replayStep : Prop :=
  ∀ (live : Bool) (rout : Job → RR) (intents : List Intent),
    replayWith replayStep live rout intents = replay live rout intents

/-- **Nothing else changes.** On every result but an input refusal the fixed
step is the old one. -/
abbrev Spec.stepInput_agrees : Prop :=
  ∀ (img : Bool) (rr : RR) (s : Streaks),
    rr ≠ .run (.err .inputRefusal) →
    stepInput img rr s = replayStep img rr s

/-- An input refusal settles `failed` with both streaks unchanged. -/
abbrev Spec.stepInput_inputRefusal : Prop :=
  ∀ (img : Bool) (s : Streaks),
    stepInput img (.run (.err .inputRefusal)) s = (.failed, s)

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

/-- **The fixed replay never stops on an input refusal**: the intent is
consumed `failed` and the loop goes on to the next one. -/
abbrev Spec.fixed_input_goes_on : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent) (e : Err),
    rout i.job = .run (.err e) →
    e.refusesInput = true →
    ∃ s', loopWith stepInput rout s (i :: rest) =
      { loopWith stepInput rout s' rest with
        answers := (i.job.key, .failed) :: (loopWith stepInput rout s' rest).answers
        log := [.consume i.job.key .failed] :: (loopWith stepInput rout s' rest).log
        failed := i.job :: (loopWith stepInput rout s' rest).failed }

/-- **The fixed replay still stops on every other non-transient error**, the
intent and every later one left unconsumed. -/
abbrev Spec.fixed_sessionWide_stops : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent) (e : Err),
    rout i.job = .run (.err e) →
    e.refusesInput = false →
    e.transient = false →
    ∃ b, loopWith stepInput rout s (i :: rest) = { ROut.empty with block := some b }

/-- The fixed replay and the in-session worker now agree on an input
refusal: both consume the intent `failed`. Stated with `workerStep`, the worker
before #295; it holds of the current worker too, whose `finishW` settles an
input refusal `failed` with the same `[consume failed]` batch (an input
refusal does not hold, `holdsS`). -/
abbrev Spec.fixed_agrees_with_worker : Prop :=
  ∀ (img : Bool) (j : Job) (s : Streaks),
    (stepInput img (.run (.err .inputRefusal)) s).1 = .failed ∧
      workerStep false j (.err .inputRefusal) = (.failed, [.consume j.key .failed])

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

/-! ### The steps -/

/-- An unparseable id settles `failed`, both streaks unchanged. -/
abbrev Spec.stepFixed_badId : Prop :=
  ∀ (img : Bool) (s : Streaks),
    stepFixed img .badId s = (.failed, s)

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

/-- **Per-agent order across serves, whatever the embedder answers.** Two
serves of the fixed replay: the first over a backlog `L1` loaded in per-epoch
sequence order, the second over any per-epoch-sorted load `L2` of the rows
the first left. Whatever each run returned (transient embedder answers
included), the writes the two serves applied, in the order they applied
them, keep each agent's writes of one process in sequence (admission) order:
the per-agent promise (`WriteIntent::agent`, `docs/reference/mcp.mdx`) that
`transient_skip_reorders` shows the code before #295 breaking. Holds for any
step; rows are keyed by receipt id (`hnd`). -/
abbrev Spec.fixed_agent_order : Prop :=
  ∀ (step : Bool → RR → Streaks → Step × Streaks) (live live' : Bool) (rout rout' : Job → RR)
    (L1 L2 : List Intent),
    (L1.map (·.job.key)).Nodup →
    EpochSorted L1 →
    EpochSorted L2 →
    (∀ i ∈ L2, i ∈ applyLog L1 (replayD step live rout L1).log) →
    ((replayD step live rout L1).applied ++ (replayD step live' rout' L2).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid)

/-- **An unparseable receipt id is settled, not skipped forever**: the fixed
loop consumes its row `failed` and goes on (`bad_id_never_settled` is the code
before #295). -/
abbrev Spec.fixed_badId_settled : Prop :=
  ∀ (rout : Job → RR) (s : Streaks) (d : List Nat) (i : Intent) (rest : List Intent),
    rout i.job = .badId →
    i.job.agent ∉ d →
    loopD stepFixed rout s d (i :: rest) =
      { loopD stepFixed rout s d rest with
        answers := (i.job.key, .failed) :: (loopD stepFixed rout s d rest).answers
        log := [.consume i.job.key .failed] :: (loopD stepFixed rout s d rest).log
        failed := i.job :: (loopD stepFixed rout s d rest).failed }

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

/-- One serve of the fixed replay over the fixed load applies each process's
writes in sequence order: `replay_epoch_seq_order` without its clock
hypothesis. -/
abbrev Spec.fixed_epoch_seq_order : Prop :=
  ∀ (live : Bool) (rout : Job → RR) (l : List Intent),
    (replayD stepFixed live rout (fixedLoad l)).applied.Pairwise
      (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid)

/-- **The two fixes together, across two serves**: rows keyed by receipt id,
loaded by the fixed load at each serve; whatever the embedder answers, the
writes the two serves apply keep each agent's writes of one process in
admission order. -/
abbrev Spec.fixed_agent_order_load : Prop :=
  ∀ (live live' : Bool) (rout rout' : Job → RR) (rows : List Intent),
    (rows.map (·.job.key)).Nodup →
    let L1 := fixedLoad rows
    let r1 := replayD stepFixed live rout L1
    ((r1.applied ++ (replayD stepFixed live' rout' (fixedLoad (applyLog L1 r1.log))).applied)).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid)

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

/-! ## The in-session hold (#295)

Before, a write the lane worker met during an embedder outage answered
`failed`, its intent was left owed, and the worker went on to the agent's next
write, which applied ahead of it; the next serve then applied the first
(`outage_then_replay_reorders`). Now the worker keeps the agent's owed writes
(`held`): at each later write of the agent it first runs the held ones,
oldest first, then the new one, and stops at the first that meets an outage.
That one and every later write of the agent stay owed: their intents
unconsumed, their receipts `pending_replay` (unsettled, so a close sets them
aside and answers `intent_durable`, and the next serve replays them in
order). A held write that is later applied in this serve answers `applied`,
a refused one `failed`; so the receipt of a write met by an outage no longer
says `failed` first and something else later.

`out t` is what a run at the `t`-th arrival returns, so a retry can answer
differently from the first attempt. The lease fence is not modelled here: it
refuses the arriving write without running anything, as before, and runs no
held one. -/

/-- Run an agent's owed jobs, oldest first, until one meets an embedder
outage: what was settled (job and answer, in order), the batches appended,
and the jobs still owed (that one and every later one). -/
def drainHeld (o : Job → RunResult) : List Job → List (Job × Answer) × List Batch × List Job
  | [] => ([], [], [])
  | j :: js =>
    match o j with
    | .ok =>
      let r := drainHeld o js
      ((j, .applied) :: r.1, [.write j.key, .consume j.key .applied] :: r.2.1, r.2.2)
    | .err e =>
      if e.transient then ([], [], j :: js)
      else
        let r := drainHeld o js
        ((j, .failed) :: r.1, [.consume j.key .failed] :: r.2.1, r.2.2)

/-- The in-session workers' state: receipts, the held jobs (every agent's, in
arrival order), the batches appended, and what was settled, in order. -/
structure HState where
  store : RStore
  held : List Job
  log : List Batch
  settled : List (Job × Answer)

def settleList (now : Int) (s : RStore) (xs : List (Job × Answer)) : RStore :=
  xs.foldl (fun s p => settleOne s p.1.key p.2 now) s

def holdList (now : Int) (s : RStore) (js : List Job) : RStore :=
  js.foldl (fun s j => settleOne s j.key .pendingReplay now) s

/-- One write `j` taken off its lane: the agent's held writes, then `j`. -/
def arriveH (o : Job → RunResult) (now : Int) (st : HState) (j : Job) : HState :=
  let r := drainHeld o (st.held.filter (fun k => k.agent = j.agent) ++ [j])
  { store := holdList now (settleList now st.store r.1) r.2.2
    held := st.held.filter (fun k => k.agent ≠ j.agent) ++ r.2.2
    log := st.log ++ r.2.1
    settled := st.settled ++ r.1 }

/-- The workers over the writes they take, in the order they take them. -/
def workH (out : Nat → Job → RunResult) (now : Int) : Nat → HState → List Job → HState
  | _, st, [] => st
  | t, st, j :: js => workH out now (t + 1) (arriveH (out t) now st j) js

/-- The writes applied in session, in the order they were applied. -/
def HState.applied (st : HState) : List Job :=
  (st.settled.filter (fun p => p.2 = .applied)).map (·.1)

/-- **The hold, over a whole serve.** For every agent, the writes it settled
in session (applied or failed) followed by the writes it still has held are
exactly its writes, in the order its lane delivered them (admission order): a
held write is never followed by a settled one, so no write of the agent is
applied while an earlier one is owed. -/
abbrev Spec.workH_split : Prop :=
  ∀ (out : Nat → Job → RunResult) (now : Int) (a : Nat),
    ∀ (js : List Job) (t : Nat) (st : HState),
    ((workH out now t st js).settled.map (·.1)).filter (fun k => k.agent = a) ++
        (workH out now t st js).held.filter (fun k => k.agent = a) =
      (st.settled.map (·.1)).filter (fun k => k.agent = a) ++
        st.held.filter (fun k => k.agent = a) ++ js.filter (fun k => k.agent = a)

/-- A serve that starts with nothing held. -/
def HState.init (s : RStore) : HState := ⟨s, [], [], []⟩

/-- `workH_split` for a serve that starts with nothing settled and nothing
held: for every agent, the writes the in-session hold settled followed by
those it still holds are that agent's writes in the order they were taken. -/
abbrev Spec.hold_split : Prop :=
  ∀ (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job) (a : Nat),
    ((workH out now 0 (HState.init s) js).settled.map (·.1)).filter (fun k => k.agent = a) ++
        (workH out now 0 (HState.init s) js).held.filter (fun k => k.agent = a) =
      js.filter (fun k => k.agent = a)

/-- **In session, each agent's writes apply in admission order, and none
before an earlier write of its agent still owed.** Writes of one process
(epoch `e`), its lanes delivering each agent's writes in sequence order. The
writes applied in session satisfy the order invariant against any rows whose
owed writes of this process are the ones still held: so the next serve,
whatever it does, keeps the order (`hold_then_serve_order`). -/
abbrev Spec.session_orderInv : Prop :=
  ∀ (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job) (e : Nat),
    (∀ j ∈ js, j.epoch = e) →
    js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid) →
    ∀ (rows : List Intent),
    (∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held) →
    OrderInv rows (workH out now 0 (HState.init s) js).applied

/-- **Per-agent order in session and across the next serve.** The writes
applied in session, then those the next serve's fixed replay applies from the
rows left (every owed row of this process being one still held), keep each
agent's writes of one process in admission order, whatever the embedder
answered at any point. The fix for `outage_then_replay_reorders`. -/
abbrev Spec.hold_then_serve_order : Prop :=
  ∀ (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job) (e : Nat),
    (∀ j ∈ js, j.epoch = e) →
    js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid) →
    ∀ (rows : List Intent),
    (rows.map (·.job.key)).Nodup →
    (∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held) →
    ∀ (live : Bool) (rout : Job → RR),
    ((workH out now 0 (HState.init s) js).applied ++
        (replayD stepFixed live rout (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid)

/-- **Progress in session**: at a write of an agent whose held writes and the
new one all get an answer other than an outage, nothing of that agent stays
held. -/
abbrev Spec.arriveH_clears : Prop :=
  ∀ (o : Job → RunResult) (now : Int) (st : HState) (j : Job),
    (∀ k ∈ st.held.filter (fun k => k.agent = j.agent) ++ [j], ∀ e, o k = .err e →
      e.transient = false) →
    (arriveH o now st j).held.filter (fun k => k.agent = j.agent) = []

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

/-- A malformed id in this build's format is still settled `failed`. -/
abbrev Spec.stepQ_badId : Prop :=
  ∀ (img : Bool) (s : Streaks),
    stepQ img .badId s = (.failed, s)

/-- An id in another format is quarantined: left owed, its agent deferred,
the streaks untouched. -/
abbrev Spec.stepQ_unknownFormat : Prop :=
  ∀ (img : Bool) (s : Streaks),
    stepQ img .unknownFormat s = (.skip, s)

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

/-- **Per-agent order across the hold, a serve with its retry pass, and
quarantine**: the writes applied in session, then by the next serve (both of
its passes), keep each agent's writes of one process in admission order,
whatever the embedder answers. -/
abbrev Spec.hold_then_serveQ_order : Prop :=
  ∀ (out : Nat → Job → RunResult) (now : Int) (s : RStore) (js : List Job) (e : Nat),
    (∀ j ∈ js, j.epoch = e) →
    js.Pairwise (fun a b => a.agent = b.agent → a.rid < b.rid) →
    ∀ (rows : List Intent),
    (rows.map (·.job.key)).Nodup →
    (∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (workH out now 0 (HState.init s) js).held) →
    ∀ (live : Bool) (rout rout' : Job → RR) (s' : Streaks),
    ((workH out now 0 (HState.init s) js).applied ++
        (serveQ live rout rout' s' (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid)

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
