/-
# Write-queue drain: the current lane worker (#295 review)

The lane worker as the Rust runs it since #295's review (`stepW`, `runW`),
with admission, the caps, drain-only retries and the lease-lost fence; the
serve that follows a session (`serveR`). It builds on `Replay.lean`'s order
invariant and retry pass. Its close is in `Close.lean`.

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
-/

import LamboProofs.Drain.Replay

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

/-! ### Lemmas about agents' sub-lists -/

theorem ofAgent_append (a : Nat) (l l' : List Job) :
    ofAgent a (l ++ l') = ofAgent a l ++ ofAgent a l' := List.filter_append ..

theorem ofAgent_cons (a : Nat) (x : Job) (l : List Job) :
    ofAgent a (x :: l) = if x.agent = a then x :: ofAgent a l else ofAgent a l := by
  by_cases h : x.agent = a <;> simp [ofAgent, h]

theorem ofAgent_nil (a : Nat) : ofAgent a [] = [] := rfl

theorem mem_ofAgent {a : Nat} {l : List Job} {x : Job} (h : x ∈ ofAgent a l) :
    x ∈ l ∧ x.agent = a := by
  simpa [ofAgent] using h

theorem head_agent {a : Nat} {l t : List Job} {h : Job} (hl : ofAgent a l = h :: t) :
    h.agent = a :=
  (mem_ofAgent (by rw [hl]; exact List.mem_cons_self)).2

theorem ofAgent_single (b : Nat) (x : Job) :
    ofAgent b [x] = if x.agent = b then [x] else [] := by
  rw [ofAgent_cons, ofAgent_nil]

theorem dropFirst_spec (sz : Job → Nat) (a : Nat) : ∀ (l : List Job) (h : Job) (t : List Job),
    ofAgent a l = h :: t →
      ofAgent a (dropFirst a l) = t ∧ (∀ b, b ≠ a → ofAgent b (dropFirst a l) = ofAgent b l) ∧
      (dropFirst a l).length + 1 = l.length ∧
      ((dropFirst a l).map sz).sum + sz h = (l.map sz).sum := by
  intro l
  induction l with
  | nil => intro h t hl; simp [ofAgent_nil] at hl
  | cons x xs ih =>
    intro h t hl
    rw [ofAgent_cons] at hl
    by_cases hx : x.agent = a
    · simp only [hx, ite_true, List.cons.injEq] at hl
      obtain ⟨rfl, rfl⟩ := hl
      refine ⟨by simp [dropFirst, hx], ?_, by simp [dropFirst, hx], by simp [dropFirst, hx]; omega⟩
      intro b hb
      simp only [dropFirst, hx, ite_true, ofAgent_cons]
      simp [Ne.symm hb]
    · simp only [hx, ite_false] at hl
      obtain ⟨h1, h2, h3, h4⟩ := ih h t hl
      simp only [dropFirst, hx, ite_false]
      refine ⟨by rw [ofAgent_cons]; simp [hx, h1], ?_, by simp; omega, by simp; omega⟩
      intro b hb
      rw [ofAgent_cons, ofAgent_cons, h2 b hb]

/-! ### Per-agent order -/

/-- For every agent: what it settled, then the write it has running (at most
one), then its held writes, then its queued writes, are exactly its admitted
writes in admission order. -/
def OrdW (st : WS) : Prop :=
  ∀ b, ofAgent b (st.settled.map (·.1)) ++ ofAgent b st.running ++ ofAgent b st.held ++
      ofAgent b st.queue = ofAgent b st.admitted ∧ (ofAgent b st.running).length ≤ 1

theorem ordW_init (s : RStore) : OrdW (WS.init s) := by
  intro b; simp [WS.init, ofAgent_nil]

/-- The common shape of every move: agent `a`'s four sub-lists change so
that their concatenation does not, and every other agent's stay. -/
theorem ordW_move {st st' : WS} {a : Nat} (hI : OrdW st) (hadm : st'.admitted = st.admitted)
    (hother : ∀ b, b ≠ a → ofAgent b (st'.settled.map (·.1)) = ofAgent b (st.settled.map (·.1)) ∧
      ofAgent b st'.running = ofAgent b st.running ∧ ofAgent b st'.held = ofAgent b st.held ∧
      ofAgent b st'.queue = ofAgent b st.queue)
    (hself : ofAgent a (st'.settled.map (·.1)) ++ ofAgent a st'.running ++ ofAgent a st'.held ++
      ofAgent a st'.queue = ofAgent a (st.settled.map (·.1)) ++ ofAgent a st.running ++
        ofAgent a st.held ++ ofAgent a st.queue)
    (hrun : (ofAgent a st'.running).length ≤ 1) : OrdW st' := by
  intro b
  by_cases hb : b = a
  · subst hb; exact ⟨by rw [hself, hadm]; exact (hI b).1, hrun⟩
  · obtain ⟨o1, o2, o3, o4⟩ := hother b hb
    rw [o1, o2, o3, o4, hadm]
    exact hI b

theorem startW_ord (now : Int) (a : Nat) (st : WS) (hI : OrdW st) : OrdW (startW now a st) := by
  unfold startW
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hI
  · have hr' : ofAgent a st.running = [] := by simpa using hr
    rw [ite_eq_right hr]
    -- The four moves, each with agent `a`'s running sub-list empty.
    have tH : ∀ h t, ofAgent a st.held = h :: t → OrdW (takeHeld a h st) := by
      intro h t hh
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hh
      have ha := head_agent hh
      refine ordW_move hI rfl (fun b hb => ⟨rfl, ?_, d2 b hb, rfl⟩) ?_ ?_
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, Ne.symm hb]
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, hr', d1, hh]
      · simp [takeHeld, ofAgent_append, ofAgent_single, ha, hr']
    have rH : ∀ h t, ofAgent a st.held = h :: t → OrdW (refuseHeld now a h st) := by
      intro h t hh
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hh
      have ha := head_agent hh
      refine ordW_move hI rfl (fun b hb => ⟨?_, rfl, d2 b hb, rfl⟩) ?_ ?_
      · simp [refuseHeld, ofAgent_append, ofAgent_single, ha, Ne.symm hb]
      · simp [refuseHeld, ofAgent_append, ofAgent_single, ha, hr', d1, hh]
      · simp [refuseHeld, hr']
    have tQ : ∀ q t, ofAgent a st.held = [] → ofAgent a st.queue = q :: t →
        OrdW (takeQueued a q st) := by
      intro q t hh hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨rfl, ?_, rfl, d2 b hb⟩) ?_ ?_
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq, hh]
      · simp [takeQueued, ofAgent_append, ofAgent_single, qa, hr']
    have rQ : ∀ q t, ofAgent a st.held = [] → ofAgent a st.queue = q :: t →
        OrdW (refuseQueued now a q st) := by
      intro q t hh hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨?_, rfl, rfl, d2 b hb⟩) ?_ ?_
      · simp [refuseQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [refuseQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq, hh]
      · simp [refuseQueued, hr']
    have pQ : ∀ q t, ofAgent a st.queue = q :: t → OrdW (parkQueued now a q st) := by
      intro q t hq
      obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.queue q t hq
      have qa := head_agent hq
      refine ordW_move hI rfl (fun b hb => ⟨rfl, rfl, ?_, d2 b hb⟩) ?_ ?_
      · simp [parkQueued, ofAgent_append, ofAgent_single, qa, Ne.symm hb]
      · simp [parkQueued, ofAgent_append, ofAgent_single, qa, hr', d1, hq]
      · simp [parkQueued, hr']
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hI
      | cons q t =>
        simp only
        split
        · exact rQ q t hh hq
        · exact tQ q t hh hq
    | cons h t =>
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · exact rH h t hh
        · split
          · exact hI
          · exact tH h t hh
      | cons q t' =>
        simp only
        split
        · exact rH h t hh
        · split
          · exact pQ q t' hq
          · exact tH h t hh

theorem finishW_ord (now : Int) (a : Nat) (r : RunResult) (st : WS) (hI : OrdW st) :
    OrdW (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hI
  | cons j t =>
    have ja := head_agent hj
    have ht : t = [] := by
      have h2 := (hI a).2
      rw [hj] at h2
      cases t with
      | nil => rfl
      | cons _ _ => simp at h2
    subst ht
    obtain ⟨d1, d2, _, _⟩ := dropFirst_spec (fun _ => 0) a st.running j [] hj
    have sR : ∀ x bt, OrdW (settleRun now a j x bt st) := by
      intro x bt
      refine ordW_move hI rfl (fun b hb => ⟨?_, d2 b hb, rfl, rfl⟩) ?_ ?_
      · simp [settleRun, ofAgent_append, ofAgent_single, ja, Ne.symm hb]
      · simp [settleRun, ofAgent_append, ofAgent_single, ja, d1, hj]
      · simp [settleRun, d1]
    simp only
    cases r with
    | ok => exact sR _ _
    | err e =>
      simp only
      split
      · refine ordW_move hI rfl (fun b hb => ⟨rfl, d2 b hb, ?_, rfl⟩) ?_ ?_
        · simp [holdRun, ofAgent_cons, ja, Ne.symm hb]
        · simp [holdRun, ofAgent_cons, ja, d1, hj]
        · simp [holdRun, d1]
      · exact sR _ _

theorem stepW_ord (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) (hI : OrdW st) :
    OrdW (stepW c sz now st ev) := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · intro b
      obtain ⟨h1, h2⟩ := hI b
      refine ⟨?_, h2⟩
      simp only [ofAgent_append, ← h1, List.append_assoc]
    · split
      · exact hI
      · exact hI
  | wake a => exact hI
  | fence => exact hI
  | start a => exact startW_ord now a st hI
  | finish a r => exact finishW_ord now a r st hI

theorem runW_ord (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), OrdW st → OrdW (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih => intro st h; exact ih _ (stepW_ord c sz now st e h)

/-! ### Caps: held writes stay counted, bytes included -/

/-- Admission's bounds hold over everything it counts. -/
def CapsW (c : Caps) (sz : Job → Nat) (st : WS) : Prop :=
  (∀ b, (ofAgent b st.load).length ≤ c.lane) ∧ st.load.length ≤ c.total ∧
    (st.load.map sz).sum ≤ c.bytes

/-- A move that never adds to what admission counts keeps the caps: per
agent, in all, and in bytes. -/
theorem capsW_shrink {c : Caps} {sz : Job → Nat} {st st' : WS} (hC : CapsW c sz st)
    (h1 : ∀ b, (ofAgent b st'.load).length ≤ (ofAgent b st.load).length)
    (h2 : st'.load.length ≤ st.load.length) (h3 : (st'.load.map sz).sum ≤ (st.load.map sz).sum) :
    CapsW c sz st' :=
  ⟨fun b => Nat.le_trans (h1 b) (hC.1 b), Nat.le_trans h2 hC.2.1, Nat.le_trans h3 hC.2.2⟩

theorem startW_caps (c : Caps) (sz : Job → Nat) (now : Int) (a : Nat) (st : WS)
    (hC : CapsW c sz st) : CapsW c sz (startW now a st) := by
  unfold startW
  -- Moving the head `x` of one of agent `a`'s lists to another, or dropping it.
  have mv : ∀ (l : List Job) (x : Job) (t : List Job), ofAgent a l = x :: t →
      (∀ b, (ofAgent b (dropFirst a l)).length + (if x.agent = b then 1 else 0) =
        (ofAgent b l).length) ∧ (dropFirst a l).length + 1 = l.length ∧
      ((dropFirst a l).map sz).sum + sz x = (l.map sz).sum := by
    intro l x t hl
    obtain ⟨d1, d2, d3, d4⟩ := dropFirst_spec sz a l x t hl
    have xa := head_agent hl
    refine ⟨?_, d3, d4⟩
    intro b
    by_cases hb : b = a
    · subst hb; rw [d1, hl]; simp [xa]
    · rw [d2 b hb]; simp [xa, Ne.symm hb]
  have cnt : ∀ (b : Nat) (x : Job), (ofAgent b [x]).length = if x.agent = b then 1 else 0 := by
    intro b x; rw [ofAgent_single]; split <;> rfl
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hC
  · rw [ite_eq_right hr]
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hC
      | cons q t =>
        obtain ⟨m1, m2, m3⟩ := mv st.queue q t hq
        simp only
        split
        · apply capsW_shrink hC <;> (try intro b) <;>
            simp only [refuseQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
              List.sum_append]
          · have := m1 b; omega
          · omega
          · omega
        · apply capsW_shrink hC <;> (try intro b) <;>
            simp only [takeQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
              List.sum_append, cnt]
          · have := m1 b; omega
          · simp; omega
          · simp; omega
    | cons h t =>
      obtain ⟨n1, n2, n3⟩ := mv st.held h t hh
      have rH : CapsW c sz (refuseHeld now a h st) := by
        apply capsW_shrink hC <;> (try intro b) <;>
          simp only [refuseHeld, WS.load, ofAgent_append, List.length_append, List.map_append,
            List.sum_append]
        · have := n1 b; omega
        · omega
        · omega
      have tH : CapsW c sz (takeHeld a h st) := by
        apply capsW_shrink hC <;> (try intro b) <;>
          simp only [takeHeld, WS.load, ofAgent_append, List.length_append, List.map_append,
            List.sum_append, cnt]
        · have := n1 b; omega
        · simp; omega
        · simp; omega
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · exact rH
        · split
          · exact hC
          · exact tH
      | cons q t' =>
        obtain ⟨m1, m2, m3⟩ := mv st.queue q t' hq
        simp only
        split
        · exact rH
        · split
          · apply capsW_shrink hC <;> (try intro b) <;>
              simp only [parkQueued, WS.load, ofAgent_append, List.length_append, List.map_append,
                List.sum_append, cnt]
            · have := m1 b; omega
            · simp; omega
            · simp; omega
          · exact tH

theorem finishW_caps (c : Caps) (sz : Job → Nat) (now : Int) (a : Nat) (r : RunResult) (st : WS)
    (hC : CapsW c sz st) : CapsW c sz (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hC
  | cons j t =>
    obtain ⟨d1, d2, d3, d4⟩ := dropFirst_spec sz a st.running j t hj
    have ja := head_agent hj
    have m1 : ∀ b, (ofAgent b (dropFirst a st.running)).length + (if j.agent = b then 1 else 0) =
        (ofAgent b st.running).length := by
      intro b
      by_cases hb : b = a
      · subst hb; rw [d1, hj]; simp [ja]
      · rw [d2 b hb]; simp [ja, Ne.symm hb]
    have sR : ∀ x bt, CapsW c sz (settleRun now a j x bt st) := by
      intro x bt
      apply capsW_shrink hC <;> (try intro b) <;>
        simp only [settleRun, WS.load, ofAgent_append, List.length_append, List.map_append,
          List.sum_append]
      · have := m1 b; omega
      · omega
      · omega
    simp only
    cases r with
    | ok => exact sR _ _
    | err e =>
      simp only
      split
      · apply capsW_shrink hC <;> (try intro b) <;>
          simp only [holdRun, WS.load, ofAgent_append, List.length_append,
            List.map_append, List.sum_append, List.length_cons, List.map_cons, List.sum_cons]
        · have hc : (ofAgent b (j :: st.held)).length =
              (ofAgent b st.held).length + (if j.agent = b then 1 else 0) := by
            rw [ofAgent_cons]; split <;> simp
          rw [hc]; have := m1 b; omega
        · omega
        · omega
      · exact sR _ _

/-- **The caps hold at every step**: queued, running and held writes, and
their bytes, never exceed the bounds. A held write stays counted from its
admission until it settles, through every retry, bytes included. -/
theorem stepW_caps (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (hC : CapsW c sz st) : CapsW c sz (stepW c sz now st ev) := by
  obtain ⟨cl, ct, cb⟩ := hC
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · rename_i hf
      simp only [fits, Bool.and_eq_true, decide_eq_true_eq] at hf
      obtain ⟨⟨f1, f2⟩, f3⟩ := hf
      refine ⟨?_, ?_, ?_⟩
      · intro b
        have := cl b
        by_cases hb : j.agent = b
        · subst hb
          simp only [WS.load, ofAgent_append, List.length_append] at f1 ⊢
          simp [ofAgent_single]; omega
        · simp only [WS.load, ofAgent_append, List.length_append] at this ⊢
          simp [ofAgent_single, hb]; omega
      · simp only [WS.load, List.length_append] at f2 ⊢; simp; omega
      · simp only [WS.load, List.map_append, List.sum_append] at f3 ⊢; simp; omega
    · split <;> exact ⟨cl, ct, cb⟩
  | wake a => exact ⟨cl, ct, cb⟩
  | fence => exact ⟨cl, ct, cb⟩
  | start a => exact startW_caps c sz now a st ⟨cl, ct, cb⟩
  | finish a r => exact finishW_caps c sz now a r st ⟨cl, ct, cb⟩

/-- The lane, queue and byte caps of `Lanes` hold over queued, running and
held writes after any sequence of worker events, and a retry never reads them
(#295). -/
theorem runW_caps (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), CapsW c sz st → CapsW c sz (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih => intro st hC; exact ih _ (stepW_caps c sz now st e hC)

/-! ### Held writes are retried without any new admission -/

/-- **The caps cannot wedge a retry**: only admission reads them, so a retry
(`wake`, `start`, `finish`) is the same under any bounds. It holds by
definition; its point is that a retry never passes through admission. -/
theorem stepW_caps_free (c c' : Caps) (sz sz' : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (h : ∀ j, ev ≠ .submit j) : stepW c sz now st ev = stepW c' sz' now st ev := by
  cases ev with
  | submit j => exact absurd rfl (h j)
  | _ => rfl

/-- In the model, a refused admission for an agent with held writes wakes its
retry: what the Rust did at `30b79824`. Since the review that followed, the
Rust lets a refusal start no retry before the backoff has passed (see the
section's header). -/
theorem refused_wakes (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (j : Job)
    (hf : fits c sz st j = false) (hh : ofAgent j.agent st.held ≠ []) :
    j.agent ∉ (stepW c sz now st (.submit j)).parked := by
  simp [stepW, hf, hh]

/-- **A retry needs no admission**: an agent with held writes and nothing
running, once its retry is due (`wake`), settles its oldest held write when
the embedder answers it, with no admission among the events. That the retry
comes due is the Rust's timer, not this theorem. -/
theorem retry_settles_head (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat)
    (h : Job) (t : List Job) (r : RunResult) (hheld : ofAgent a st.held = h :: t)
    (hrun : ofAgent a st.running = []) (hf : st.fenced = false)
    (hr : ∀ e, r = .err e → holdsS e = false) :
    ofAgent a (runW c sz now st [.wake a, .start a, .finish a r]).held = t ∧
      (runW c sz now st [.wake a, .start a, .finish a r]).settled =
        st.settled ++ [(h, if r = .ok then .applied else .failed)] := by
  obtain ⟨d1, _, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hheld
  have ha := head_agent hheld
  have hs : startW now a { st with parked := st.parked.filter (· ≠ a) } =
      takeHeld a h { st with parked := st.parked.filter (· ≠ a) } := by
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hrun]
    rw [ite_eq_right hn]
    simp only [hheld, hf, Bool.false_eq_true, ite_false]
    cases hq : ofAgent a st.queue <;> simp
  have hr2 : ofAgent a (st.running ++ [h]) = [h] := by
    simp [ofAgent_append, hrun, ofAgent_single, ha]
  simp only [runW, stepW]
  rw [hs]
  cases r with
  | ok => simp [finishW, takeHeld, settleRun, hr2, d1]
  | err e =>
    have he := hr e rfl
    simp [finishW, takeHeld, settleRun, hr2, d1, he]

/-- **A retry is always available while writes are held**: for an agent
with held writes, nothing running and the lease held, the events `wake` then
`start` take its oldest held write to run, whatever the parking, the caps or
the rest of the state. (Enabledness, not fairness: that the environment
delivers `wake` is the Rust's timer.) -/
theorem retry_enabled (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (a : Nat) (h : Job)
    (t : List Job) (hheld : ofAgent a st.held = h :: t) (hrun : ofAgent a st.running = [])
    (hf : st.fenced = false) :
    (runW c sz now st [.wake a, .start a]).running = st.running ++ [h] ∧
      ofAgent a (runW c sz now st [.wake a, .start a]).held = t := by
  obtain ⟨d1, _, _, _⟩ := dropFirst_spec (fun _ => 0) a st.held h t hheld
  have hs : startW now a { st with parked := st.parked.filter (· ≠ a) } =
      takeHeld a h { st with parked := st.parked.filter (· ≠ a) } := by
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hrun]
    rw [ite_eq_right hn]
    simp only [hheld, hf, Bool.false_eq_true, ite_false]
    cases hq : ofAgent a st.queue <;> simp
  simp only [runW, stepW]
  rw [hs]
  exact ⟨rfl, d1⟩

/-! ### The fence -/

theorem stepW_fenced (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev)
    (hf : st.fenced = true) (hr : st.running = []) :
    (stepW c sz now st ev).fenced = true ∧ (stepW c sz now st ev).running = [] ∧
      (stepW c sz now st ev).applied = st.applied ∧ (stepW c sz now st ev).log = st.log := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact ⟨hf, hr, rfl, rfl⟩
    · split
      · exact ⟨hf, hr, rfl, rfl⟩
      · exact ⟨hf, hr, rfl, rfl⟩
  | wake a => exact ⟨hf, hr, rfl, rfl⟩
  | fence => exact ⟨rfl, hr, rfl, rfl⟩
  | finish a r => simp [stepW, finishW, hr, ofAgent_nil, hf]
  | start a =>
    simp only [stepW]
    unfold startW
    have hn : ¬ ofAgent a st.running ≠ [] := by simp [hr, ofAgent_nil]
    rw [ite_eq_right hn]
    cases hh : ofAgent a st.held <;> cases hq : ofAgent a st.queue <;>
      simp [refuseHeld, refuseQueued, WS.applied, hf, hr]

/-- **No write taken after the fence is applied.** Once the lease is lost
(latched) and nothing is running, no later event applies or logs anything:
every held or queued write is refused, oldest first, without being run. A
write already running when the fence landed is the only one that can still
commit, and the store refuses that flush. -/
theorem fence_stops_applies (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), st.fenced = true → st.running = [] →
      (runW c sz now st evs).applied = st.applied ∧ (runW c sz now st evs).log = st.log := by
  intro evs
  induction evs with
  | nil => intro st _ _; exact ⟨rfl, rfl⟩
  | cons e es ih =>
    intro st hf hr
    obtain ⟨f1, f2, f3, f4⟩ := stepW_fenced c sz now st e hf hr
    obtain ⟨g1, g2⟩ := ih _ f1 f2
    exact ⟨g1.trans f3, g2.trans f4⟩

/-! ### Order across the session and the next serve -/

/-- **Per-agent order in session, with admission and retries.** The writes
applied in session satisfy the order invariant against any rows whose owed
writes of this process are still running, held or queued. -/
theorem sessionW_orderInv (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore) (evs : List Ev)
    (e : Nat) (he : ∀ j ∈ (runW c sz now (WS.init s) evs).admitted, j.epoch = e)
    (hjs : (runW c sz now (WS.init s) evs).admitted.Pairwise
      (fun a b => a.agent = b.agent → a.rid < b.rid))
    (rows : List Intent)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (runW c sz now (WS.init s) evs).running ∨
      i.job ∈ (runW c sz now (WS.init s) evs).held ∨
      i.job ∈ (runW c sz now (WS.init s) evs).queue) :
    OrderInv rows (runW c sz now (WS.init s) evs).applied := by
  let st := runW c sz now (WS.init s) evs
  have hO : OrdW st := runW_ord c sz now evs _ (ordW_init s)
  have hsub : st.applied.Sublist (st.settled.map (·.1)) := (List.filter_sublist).map _
  have hja : ∀ a, (ofAgent a st.admitted).Pairwise (fun x y => x.rid < y.rid) := by
    intro a
    apply List.Pairwise.imp_of_mem _ (hjs.sublist List.filter_sublist)
    intro x y hx hy hxy
    exact hxy (by rw [(mem_ofAgent hx).2, (mem_ofAgent hy).2])
  have hsettled_mem : ∀ x ∈ st.settled.map (·.1), x ∈ st.admitted := by
    intro x hx
    have h1 : x ∈ ofAgent x.agent (st.settled.map (·.1)) := by simp [ofAgent, hx]
    have h2 := (hO x.agent).1
    have : x ∈ ofAgent x.agent st.admitted := by
      rw [← h2]; simp only [List.append_assoc]; exact List.mem_append_left _ h1
    exact (mem_ofAgent this).1
  refine ⟨?_, ?_⟩
  · apply List.Pairwise.sublist hsub
    apply pairwise_of_agent_filters
    intro a
    have hpre : (ofAgent a (st.settled.map (·.1))).Sublist (ofAgent a st.admitted) := by
      rw [← (hO a).1]; simp only [List.append_assoc]; exact List.sublist_append_left _ _
    exact ((hja a).sublist hpre).imp
      (fun {x y} (h : x.rid < y.rid) (_ : x.epoch = y.epoch) => Nat.le_of_lt h)
  · intro x hx i hi hio hag hep
    have hxs := hsub.subset hx
    have hxe : x.epoch = e := he x (hsettled_mem x hxs)
    have hpa := hja x.agent
    rw [← (hO x.agent).1] at hpa
    simp only [List.append_assoc] at hpa
    have hcross := (List.pairwise_append.mp hpa).2.2
    have hxa : x ∈ ofAgent x.agent (st.settled.map (·.1)) := by simp [ofAgent, hxs]
    have hio' : i.job ∈ ofAgent x.agent st.running ++ (ofAgent x.agent st.held ++
        ofAgent x.agent st.queue) := by
      have hm : ∀ l : List Job, i.job ∈ l → i.job ∈ ofAgent x.agent l :=
        fun l h => List.mem_filter.mpr ⟨h, by simp [hag]⟩
      rcases hrows i hi hio (hep ▸ hxe) with h | h | h
      · exact List.mem_append_left _ (hm _ h)
      · exact List.mem_append_right _ (List.mem_append_left _ (hm _ h))
      · exact List.mem_append_right _ (List.mem_append_right _ (hm _ h))
    exact Nat.le_of_lt (hcross x hxa i.job hio')

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
theorem serveR_reported (live : Bool) (rout rout' : Job → RR) (s' : Streaks)
    (intents : List Intent)
    (h : pendingOf (applyLog intents (serveR live rout rout' s' intents).log) ≠ []) :
    reported (serveR live rout rout' s' intents).block ≠ none := by
  unfold serveR at h ⊢
  dsimp only at h ⊢
  by_cases hc : (serveQ live rout rout' s' intents).block = some .unset ∧
      pendingOf (applyLog intents (serveQ live rout rout' s' intents).log) ≠ []
  · rw [ite_eq_left hc]; simp [reported]
  · rw [ite_eq_right hc] at h ⊢
    have hb := serveQ_block live rout rout' s' intents h
    cases hq : (serveQ live rout rout' s' intents).block with
    | none => exact absurd hq hb
    | some b =>
      cases b with
      | unset => exact absurd ⟨hq, h⟩ hc
      | _ => simp [reported]

theorem serveR_log (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) :
    (serveR live rout rout' s' intents).log = (serveQ live rout rout' s' intents).log ∧
      (serveR live rout rout' s' intents).applied = (serveQ live rout rout' s' intents).applied := by
  unfold serveR
  dsimp only
  split <;> exact ⟨rfl, rfl⟩

/-- **Per-agent order from the session (admission, caps, retries) through
the next serve (both passes)**, whatever the embedder answers. -/
theorem sessionW_then_serve_order (c : Caps) (sz : Job → Nat) (now : Int) (s : RStore)
    (evs : List Ev) (e : Nat)
    (he : ∀ j ∈ (runW c sz now (WS.init s) evs).admitted, j.epoch = e)
    (hjs : (runW c sz now (WS.init s) evs).admitted.Pairwise
      (fun a b => a.agent = b.agent → a.rid < b.rid))
    (rows : List Intent) (hnd : (rows.map (·.job.key)).Nodup)
    (hrows : ∀ i ∈ rows, i.outcome = none → i.job.epoch = e →
      i.job ∈ (runW c sz now (WS.init s) evs).running ∨
      i.job ∈ (runW c sz now (WS.init s) evs).held ∨
      i.job ∈ (runW c sz now (WS.init s) evs).queue)
    (live : Bool) (rout rout' : Job → RR) (s' : Streaks) :
    ((runW c sz now (WS.init s) evs).applied ++
        (serveR live rout rout' s' (fixedLoad rows)).applied).Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hI := orderInv_perm (fixedLoad_perm rows)
    (sessionW_orderInv c sz now s evs e he hjs rows hrows)
  have hnd' : ((fixedLoad rows).map (·.job.key)).Nodup :=
    ((fixedLoad_perm rows).map _).nodup_iff.mpr hnd
  rw [(serveR_log live rout rout' s' _).2, (serveQ_log live rout rout' s' _).2]
  exact (replayP_preserves stepQ live rout rout' s' _ _ hnd' (fixedLoad_sorted rows) hI).1

end Lambo.Drain
