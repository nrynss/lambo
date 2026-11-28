/-
# Write-queue drain: the current lane worker (#295 review)

The lane worker as the Rust runs it since #295's review (`stepW`, `runW`),
with admission, the caps, drain-only retries and the lease-lost fence; the
serve that follows a session (`serveR`). It builds on `Replay.lean`'s order
invariant and retry pass. Its close is in `Close.lean`.

This is the specification layer: the definitions and each result as a
proposition `Spec.<name>` (`Spec/Base.lean` describes the two layers). The
proofs are in `Drain/Proofs/Worker.lean`.

## Results

* Every event is a step (an admission, one write taken, one write finished, a
  drain-only retry, the fence). `stepW_ord`, `runW_ord`: per agent, settled,
  running, held and queued writes are its admitted writes in order;
  `stepW_caps`, `runW_caps`: the lane, queue and byte caps hold over queued,
  running and held writes at every step; `retry_settles_head`,
  `retry_enabled`: a retry needs no admission, is always available while
  writes are held, and settles the oldest held write once the embedder
  answers (that the retry comes due is the Rust's timer, not modelled);
  `stepW_caps_free`: a retry does not read the caps, by definition;
  `refused_wakes`: the commit at which a refused write woke the retry at
  once; `fence_stops_applies`: after the fence nothing taken is applied;
  `sessionW_orderInv`, `sessionW_then_serve_order`: per-agent order from the
  session through the next serve. `serveR_reported`: a halt that leaves
  intents owed reports `other`, never `null` (`serveQ_block` read `unset` as
  a reason; the stats print it as `null`).

Boundary: each event is atomic, as in `Base.lean`; when a retry comes due is
the Rust's timer, not modelled; a run's result is an input per write. A
worker that panics after its commit is `Close.lean`'s (`stepK`, `stepP`), not
`stepW`'s.
Startup replay admission and restart-required parking are outside the lane
events below. The writer refuses fresh admissions while replay is staging or
unconfirmed, and a lane parked for restart keeps its intents owed without
timed retries.
-/

import LamboProofs.Drain.Spec.Replay

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## The hold with admission, caps, retries and the fence (#295 review)

`workH` treats a worker's whole pass as one step and has no admission, so it
could not show two faults review found: held writes counted against
admission while the only retry trigger was a new admitted write (they could
be stranded), and a retry ran several held writes after one lease check.
This model has the events themselves. `submit j`: admission, against the
caps, counting queued, running and held writes and their bytes. `start a`:
agent `a`'s worker takes one write: its oldest held write unless it is
parked after an outage, else its next queued write, which joins the held
ones if any are left. `finish a r`: that write's run returns `r`. `wake a`:
a drain-only retry (the backoff timer, a new write, or a refused admission
for an agent with held writes) ends the parking. `fence`: the lease is lost
(latched, as #293 makes it). Every step is atomic; a write in flight is
counted in `running`.

The fence is checked when a write is taken, before it runs: a write taken
after the fence is refused (`failed`, its intent left to the next holder)
and never run; a write already running when the fence lands can still
commit, which the store's fenced flush then refuses (`Flush/Loop.lean`).

Which errors hold a write (`holdsS`): an embedder outage. A lookup timeout
settles `failed`, as the replay settles a single one; the worker's run of
them that says the store is slow is handed to the model as `unavailable`
(the class the worker acts on, not its cause). The run counts first attempts
only: a held write's retry that times out again is handed over as
`lookupTimeout`, and settles.

**What the retry theorems cover, and what they do not.** `wake` and `start`
are free events of the environment, so `retry_settles_head` and
`retry_enabled` say that a retry needs no admission and is never blocked by
the caps (`stepW_caps_free`, which holds because only admission reads them),
and that it is always available while an agent has held writes and nothing
running. They do not say that a retry ever happens: that is the Rust's
backoff timer, which a worker keeps while its agent has held writes
(`execution::held_retry_backoff`), and timing is not modelled. Nor do they
bound how often it happens: in the Rust, neither a new write nor a refused
admission starts a retry before the backoff since the last attempt has
passed. Such a refusal is therefore no event in the model at all: it changes
nothing but the parking, which in the Rust it leaves alone. `refused_wakes`
describes the commit before that (`30b79824`), where a refusal woke the
retry at once. -/

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

/-- The workers' state: receipts, queued, running and held writes, the
parked agents, the latched fence, the batches appended, what was settled (in
order), and every admitted write (in admission order). -/
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

/-- Admission's three checks, every one counting held writes. -/
def fits (c : Caps) (sz : Job → Nat) (st : WS) (j : Job) : Bool :=
  decide ((ofAgent j.agent st.load).length < c.lane) && decide (st.load.length < c.total) &&
    decide ((st.load.map sz).sum + sz j ≤ c.bytes)

/-- The errors that hold a write in session. -/
def holdsS (e : Err) : Bool := e == .unavailable

/-! The worker's moves, one write each. -/

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

/-! ### Per-agent order -/

/-- For every agent: what it settled, then the write it has running (at most
one), then its held writes, then its queued writes, are exactly its admitted
writes in admission order. -/
def OrdW (st : WS) : Prop :=
  ∀ b, ofAgent b (st.settled.map (·.1)) ++ ofAgent b st.running ++ ofAgent b st.held ++
      ofAgent b st.queue = ofAgent b st.admitted ∧ (ofAgent b st.running).length ≤ 1

/-- Any sequence of worker events keeps `OrdW`, so an agent's writes are
settled in admission order, held writes and retries included. -/
abbrev Spec.runW_ord : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), OrdW st → OrdW (runW c sz now st evs)

/-! ### Caps: held writes stay counted, bytes included -/

/-- Admission's bounds hold over everything it counts. -/
def CapsW (c : Caps) (sz : Job → Nat) (st : WS) : Prop :=
  (∀ b, (ofAgent b st.load).length ≤ c.lane) ∧ st.load.length ≤ c.total ∧
    (st.load.map sz).sum ≤ c.bytes

/-- The lane, queue and byte caps of `Lanes` hold over queued, running and
held writes after any sequence of worker events, and a retry never reads them
(#295). -/
abbrev Spec.runW_caps : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), CapsW c sz st → CapsW c sz (runW c sz now st evs)

/-! ### Held writes are retried without any new admission -/

/-- **A retry needs no admission**: an agent with held writes and nothing
running, once its retry is due (`wake`), settles its oldest held write when
the embedder answers it, with no admission among the events. That the retry
comes due is the Rust's timer, not this theorem. -/
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

/-- **A retry is always available while writes are held**: for an agent
with held writes, nothing running and the lease held, the events `wake` then
`start` take its oldest held write to run, whatever the parking, the caps or
the rest of the state. (Enabledness, not fairness: that the environment
delivers `wake` is the Rust's timer.) -/
abbrev Spec.retry_enabled : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat) (h : Job) (t : List Job),
    ofAgent a st.held = h :: t →
    ofAgent a st.running = [] →
    st.fenced = false →
    (runW c sz now st [.wake a, .start a]).running = st.running ++ [h] ∧
      ofAgent a (runW c sz now st [.wake a, .start a]).held = t

/-! ### The fence -/

/-- **No write taken after the fence is applied.** Once the lease is lost
(latched) and nothing is running, no later event applies or logs anything:
every held or queued write is refused, oldest first, without being run. A
write already running when the fence landed is the only one that can still
commit, and the store refuses that flush. -/
abbrev Spec.fence_stops_applies : Prop :=
  ∀ (c : Caps) (sz : Job → Nat) (now : Int),
    ∀ (evs : List Ev) (st : WS), st.fenced = true → st.running = [] →
      (runW c sz now st evs).applied = st.applied ∧ (runW c sz now st evs).log = st.log

/-! ### Order across the session and the next serve -/

/-! ### The replay's block reason after a halt -/

/-- What `write_queue_replay_blocked` reports for a block reason: `unset` (a
halt on a lost lease or a seal) reads `null`, like no block. -/
def reported : Option Block → Option Block
  | some .unset => none
  | b => b

/-- The serve after the review: a halt that leaves intents owed reports
`other`, as a lost lease is an `other` fault. -/
def serveR (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) : ROut :=
  let r := serveQ live rout rout' s' intents
  if r.block = some .unset ∧ pendingOf (applyLog intents r.log) ≠ [] then
    { r with block := some .other }
  else r

/-- **The reported block reason is never `null` while intents are owed**,
halts included (`serveQ_block` holds only before `unset` is read as `null`). -/
abbrev Spec.serveR_reported : Prop :=
  ∀ (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent),
    pendingOf (applyLog intents (serveR live rout rout' s' intents).log) ≠ [] →
    reported (serveR live rout rout' s' intents).block ≠ none

/-- **Per-agent order from the session (admission, caps, retries) through
the next serve (both passes)**, whatever the embedder answers. -/
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
