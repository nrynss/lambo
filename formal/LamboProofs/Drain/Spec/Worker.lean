/-
# Write-queue drain: the lane worker

The lane worker as events (`Ev`, `stepW`, `runW`): an admission against the
caps (`submit`), a worker taking one write (`start`: its agent's oldest held
write unless parked, else its next queued write, which joins the held ones if
any are left), that write's run ending (`finish`), a drain-only retry ending
the parking (`wake`), and the latched lease-lost fence (`fence`); then the
serve that follows a session (`serveR`).

Rust: `src/writeq/execution.rs` (`WritePipeline::spawn_worker`, the hold
arm, `held_retry_backoff`); `Lanes` and admission in `WritePipeline`
(`src/writeq/admission.rs`); `settle_one`, `hold_one`
(`src/writeq/receipts.rs`); `WritePipeline::spawn_replay`
(`src/writeq/replay.rs`).

Results:
* `runW_ord`: per agent, settled, running, held and queued writes are its admitted writes in order.
* `runW_caps`: the lane, queue and byte caps hold over queued, running and held writes.
* `retry_settles_head`, `retry_enabled`: a retry needs no admission, is available while
  writes are held, and settles the oldest held write once the embedder answers.
* `fence_stops_applies`: after the fence nothing taken is applied.
* `serveR_reported`: the reported block reason is never `null` while intents are owed.
* `sessionW_then_serve_order`: per-agent order from the session through the next serve.

Boundary: each event is atomic and a run's result is an input per write.
When a retry comes due is the Rust's backoff timer, which is not modelled:
`wake` and `start` are free events, so the retry results state enabledness,
not that a retry happens. A refused admission unparks its agent in `stepW`,
which the Rust does only at the backoff; the Rust's refusal changes nothing,
which is the model's trace without that event. The hold decision is an input:
a run of lookup timeouts that holds a write is handed over as `unavailable`
(`Close.lean`'s `heldErrW`). A write running when the fence lands can still
commit, and the store refuses that flush. A worker that panics after its
commit is `Close.lean`'s panic model, not `stepW`'s. Startup replay
admission and restart-required parking are outside these events: the writer
refuses fresh admissions while replay is staging or unconfirmed, and a lane
parked for restart keeps its intents owed without timed retries.
-/

import LamboProofs.Drain.Spec.Replay

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The workers' state and events -/

/-- The admission bounds: per agent (`WRITE_QUEUE_LANE_MAX`), the whole queue
(`WRITE_QUEUE_MAX`), payload bytes (`WRITE_QUEUE_MAX_BYTES`). -/
structure Caps where
  lane : Nat
  total : Nat
  bytes : Nat

/-- An agent's writes in a list, in order. -/
def ofAgent (a : Nat) (l : List Job) : List Job := l.filter (fun k => k.agent = a)

/-- The list without the first write of agent `a`. -/
def dropFirst (a : Nat) : List Job → List Job
  | [] => []
  | x :: xs => if x.agent = a then xs else x :: dropFirst a xs

/-- The workers' state (`Lanes`): receipts, queued, running and held writes,
parked agents, the fence, batches appended, what was settled and admitted. -/
structure WS where
  store : RStore
  queue : List Job
  running : List Job
  held : List Job
  parked : List Nat
  fenced : Bool
  log : List Batch
  settled : List (Job × Answer)
  admitted : List Job

def WS.init (s : RStore) : WS := ⟨s, [], [], [], [], false, [], [], []⟩

/-- What admission counts: queued, running and held writes. -/
def WS.load (st : WS) : List Job := st.queue ++ st.running ++ st.held

def WS.applied (st : WS) : List Job := (st.settled.filter (fun p => p.2 = .applied)).map (·.1)

inductive Ev where
  | submit (j : Job)
  | start (a : Nat)
  | finish (a : Nat) (r : RunResult)
  | wake (a : Nat)
  | fence
  deriving DecidableEq, Repr

/-- Admission's three checks (`admission.rs`), every one counting held writes. -/
def fits (c : Caps) (sz : Job → Nat) (st : WS) (j : Job) : Bool :=
  decide ((ofAgent j.agent st.load).length < c.lane) && decide (st.load.length < c.total) &&
    decide ((st.load.map sz).sum + sz j ≤ c.bytes)

/-- The errors that hold a write in session (`spawn_worker`'s hold arm). -/
def holdsS (e : Err) : Bool := e == .unavailable

/-! ## The worker's moves (`spawn_worker`), one write each -/

/-- Take the oldest held write `h` to run. -/
def takeHeld (a : Nat) (h : Job) (st : WS) : WS :=
  { st with held := dropFirst a st.held, running := st.running ++ [h] }

/-- Take the next queued write `q` to run (nothing held). -/
def takeQueued (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, running := st.running ++ [q] }

/-- A parked agent's next queued write `q` joins its held writes. -/
def parkQueued (now : Int) (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, held := st.held ++ [q],
            store := settleOne st.store q.key .pendingReplay now }

/-- After the fence: refuse the oldest held write, never run. -/
def refuseHeld (now : Int) (a : Nat) (h : Job) (st : WS) : WS :=
  { st with held := dropFirst a st.held, settled := st.settled ++ [(h, .failed)],
            store := settleOne st.store h.key .failed now }

/-- After the fence: refuse the next queued write, never run. -/
def refuseQueued (now : Int) (a : Nat) (q : Job) (st : WS) : WS :=
  { st with queue := dropFirst a st.queue, settled := st.settled ++ [(q, .failed)],
            store := settleOne st.store q.key .failed now }

/-- The running write `j` settles with `x`, appending `b`. -/
def settleRun (now : Int) (a : Nat) (j : Job) (x : Answer) (b : Batch) (st : WS) : WS :=
  { st with running := dropFirst a st.running, settled := st.settled ++ [(j, x)],
            log := st.log ++ [b], store := settleOne st.store j.key x now }

/-- The running write `j` met an outage: it becomes the oldest held write and
the agent is parked until a retry wakes it. -/
def holdRun (now : Int) (a : Nat) (j : Job) (st : WS) : WS :=
  { st with running := dropFirst a st.running, held := j :: st.held, parked := a :: st.parked,
            store := settleOne st.store j.key .pendingReplay now }

def startW (now : Int) (a : Nat) (st : WS) : WS :=
  if ofAgent a st.running ≠ [] then st
  else
    match ofAgent a st.held, ofAgent a st.queue with
    | h :: _, q :: _ =>
      if st.fenced then refuseHeld now a h st
      else if a ∈ st.parked then parkQueued now a q st
      else takeHeld a h st
    | h :: _, [] =>
      if st.fenced then refuseHeld now a h st
      else if a ∈ st.parked then st
      else takeHeld a h st
    | [], q :: _ => if st.fenced then refuseQueued now a q st else takeQueued a q st
    | [], [] => st

def finishW (now : Int) (a : Nat) (r : RunResult) (st : WS) : WS :=
  match ofAgent a st.running with
  | [] => st
  | j :: _ =>
    match r with
    | .ok => settleRun now a j .applied [.write j.key, .consume j.key .applied] st
    | .err e =>
      if holdsS e then holdRun now a j st
      else settleRun now a j .failed [.consume j.key .failed] st

def stepW (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) : Ev → WS
  | .submit j =>
    if fits c sz st j then { st with queue := st.queue ++ [j], admitted := st.admitted ++ [j] }
    -- A refused write of an agent with held writes wakes its retry.
    else if ofAgent j.agent st.held ≠ [] then { st with parked := st.parked.filter (· ≠ j.agent) }
    else st
  | .start a => startW now a st
  | .finish a r => finishW now a r st
  | .wake a => { st with parked := st.parked.filter (· ≠ a) }
  | .fence => { st with fenced := true }

def runW (c : Caps) (sz : Job → Nat) (now : Int) : WS → List Ev → WS
  | st, [] => st
  | st, e :: es => runW c sz now (stepW c sz now st e) es

/-! ## Results -/

/-- Per agent: settled, then running (at most one), held and queued writes are
its admitted writes in admission order. -/
def OrdW (st : WS) : Prop :=
  ∀ b, ofAgent b (st.settled.map (·.1)) ++ ofAgent b st.running ++ ofAgent b st.held ++
      ofAgent b st.queue = ofAgent b st.admitted ∧ (ofAgent b st.running).length ≤ 1

/-- **An agent's writes settle in admission order**, held writes and retries
included. Rust test `an_outage_in_session_holds_the_agents_later_writes_behind_the_owed_one`. -/
abbrev Spec.runW_ord : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), OrdW st → OrdW (runW c sz now st evs)

/-- Admission's bounds hold over everything it counts. -/
def CapsW (c : Caps) (sz : Job → Nat) (st : WS) : Prop :=
  (∀ b, (ofAgent b st.load).length ≤ c.lane) ∧ st.load.length ≤ c.total ∧
    (st.load.map sz).sum ≤ c.bytes

/-- **The lane, queue and byte caps hold over queued, running and held
writes**, bytes included. Rust test `held_writes_keep_their_bytes_counted_until_they_settle`. -/
abbrev Spec.runW_caps : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), CapsW c sz st → CapsW c sz (runW c sz now st evs)

/-- **A retry needs no admission**: once due, it settles the agent's oldest held
write when the embedder answers. Rust test
`held_writes_are_retried_without_a_new_write_even_when_the_lane_is_full`. -/
abbrev Spec.retry_settles_head : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat) (h : Job) (t : List Job)
    (r : RunResult),
    ofAgent a st.held = h :: t →
    ofAgent a st.running = [] →
    st.fenced = false →
    (∀ e, r = .err e → holdsS e = false) →
    ofAgent a (runW c sz now st [.wake a, .start a, .finish a r]).held = t ∧
      (runW c sz now st [.wake a, .start a, .finish a r]).settled =
        st.settled ++ [(h, if r = .ok then .applied else .failed)]

/-- **A retry is available while writes are held**: with nothing running and
the lease held, `wake` then `start` run the oldest held write, whatever the caps. -/
abbrev Spec.retry_enabled : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat) (h : Job) (t : List Job),
    ofAgent a st.held = h :: t →
    ofAgent a st.running = [] →
    st.fenced = false →
    (runW c sz now st [.wake a, .start a]).running = st.running ++ [h] ∧
      ofAgent a (runW c sz now st [.wake a, .start a]).held = t

/-- **No write taken after the fence is applied**: once the lease is lost and
nothing is running, no event applies or logs anything. -/
abbrev Spec.fence_stops_applies : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), st.fenced = true → st.running = [] →
      (runW c sz now st evs).applied = st.applied ∧ (runW c sz now st evs).log = st.log

/-! ## The serve after a session -/

/-- What `write_queue_replay_blocked` reports: `unset` (a halt on a lost lease
or a seal) reads `null`. -/
def reported : Option Block → Option Block
  | some .unset => none
  | b => b

/-- `spawn_replay`'s serve: a halt that leaves intents owed reports `other`. -/
def serveR (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := serveQ live rout rout' s' intents
  if r.block = some .unset ∧ pendingOf (applyLog intents r.log) ≠ [] then
    { r with block := some .other }
  else r

/-- **The reported block reason is never `null` while intents are owed**,
halts included. Rust test `replay_blocked_names_the_reason_or_is_none`. -/
abbrev Spec.serveR_reported : Prop :=
  ∀ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent),
    pendingOf (applyLog intents (serveR live rout rout' s' intents).log) ≠ [] →
    reported (serveR live rout rout' s' intents).block ≠ none

/-- **Each agent's writes of one process apply in admission order from the
session through the next serve**, whatever the embedder answers. -/
abbrev Spec.sessionW_then_serve_order : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore) (evs : List Ev) (e : Nat),
    (∀ j ∈ (runW c sz now (WS.init s) evs).admitted, j.epoch = e) →
    (runW c sz now (WS.init s) evs).admitted.Pairwise
      (fun a b => a.agent = b.agent → a.rid < b.rid) →
    ∀ (rows : List Intent),
    (rows.map (·.job.key)).Nodup →
    (∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (runW c sz now (WS.init s) evs).running ∨
      i.job ∈ (runW c sz now (WS.init s) evs).held ∨
      i.job ∈ (runW c sz now (WS.init s) evs).queue) →
    ∀ (live : Bool) (rout rout' : Job → RR) (s' : Streaks),
    ((runW c sz now (WS.init s) evs).applied ++
        (serveR live rout rout' s' (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid)

end Lambo.Drain
