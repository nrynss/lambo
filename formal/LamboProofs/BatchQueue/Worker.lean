import LamboProofs.BatchQueue.Pure

/-
# The candle embedder's batch queue: the worker and its callers (#125)

Models the coalescer of `src/embed/candle.rs` as a state machine over
`Pure.lean`'s pieces: `Pending` and `BatchQueue` (`push`, `push_many`,
`request_shutdown`, `wait_for_batch`, `top_up`, `drain_all`),
`spawn_coalescer`, `supervise` and `coalesce_loop` (the take, the debounce and
its top-up, the exit with `drain_all`), `serve_take` with `still_awaited` and
the send loop, and the caller side: `embed` pushes one single and waits in
`recv_bounded`, `embed_batch` pushes its texts as bulk under one lock, and
`Handle::drop` requests shutdown when the last handle goes. `St` holds the
queue, the shutdown flag, the worker's position (`Pc`), the supervisor and
`Shared` liveness, and the bookkeeping the theorems read (every request
queued, the callers that gave up, what each observed, the forwards run).
`step` takes one event: a push (`enqueue`, `enqueueMany`), a caller's
deadline (`giveUp`), the last handle's drop (`shutdown`), one worker step
(`work`, deterministic), the worker's death (`die`), the supervisor's
reaction (`supervise`, with a spawn that may fail) and the drop of `Shared`
(`dropShared`). Each `work` step is one lock hold, one forward with the pull
before it, or one `send`; `enqueue` needs no shutdown, since an awaiting
`embed()` borrows the embedder.

Results: in every run no caller observes twice (`answered_at_most_once`); the
invariant keeps every request in exactly one place, across deaths and
restarts (`Good`, `good_reachable`); once the worker has caught up every
request ever queued has been observed exactly once (`core_at_rest`), and a
running worker left alone catches up within `measure` steps
(`served_when_left_alone`, `work_progress`, `workN_idle`). After shutdown a
running worker exits only once nothing is queued or held, having answered
everything, and no singles are pulled once shutdown is requested
(`shutdown_drains`, `pullOf_nil`). `eventually_all_answered` is an accounting
theorem only: shutdown, the drain, the supervisor's exit and the drop of
`Shared` leave every request observed once, but in a wedged or dead state as
`.dropped`, and the recipe requests shutdown while callers may wait, which the
Rust cannot do. No cross-wiring: the answers being sent are, position by
position, `forward_isolated`'s results on their own texts (`answers_wired`),
and with a row-wise forward every answer a caller receives is what its own
text gets alone (`answers_are_own`). Every forward holds at most
`FORWARD_WIDTH` texts and a take with its top-up at most `MAX_BATCH`
(`forwards_capped`, `topUp_cap`); the serving step's pulls obey `drive`'s
discipline, so `take_ends` bounds every take (`pullOf_drive`).

Boundary: the debounce's duration and every timing bound are not modelled
(only order); `EMBED_WAIT` and deadlines are events, not times; poisoned
locks behave as unpoisoned ones (`unpoison`); the empty-text rejection
(`reject_empty`, never queued), loading (`load`, `prepare`, `LoadSlot`), the
panic message text and memory are left out: a death is an event, not a cause.
`answers_are_own` is exact in the model only (see `Pure.lean`'s boundary).
#273 Track 3 proposes `candle/queue.rs`, `candle/model.rs` and
`candle/identity.rs`; the caller-side theorems (`answered_at_most_once`,
`eventually_all_answered`) would span `queue.rs` and `mod.rs`.
-/

namespace Lambo.BatchQueue

open Lambo.Lib (eraseIdx_perm map_eq_pointwise)

/-! ## The state machine -/

/-- What a caller observes, once: the answer its oneshot carried (`res`), the
`RecvError` of a sender dropped unsent (`dropped`: `recv_bounded` and
`embed_batch` turn it into `Backend("candle coalescer dropped the request
(worker exited mid-batch)")`), or its own deadline (`timedOut`: `Unavailable`
from `recv_bounded`, `deadline_passed()` from `embed_batch`). -/
inductive Obs where
  | res (r : Res)
  | dropped
  | timedOut
  deriving DecidableEq, Repr

/-- The error classes a caller reports (`EmbedError::is_transient`):
`some true` transient, `some false` permanent, `none` a vector. -/
def Obs.transient : Obs → Option Bool
  | .res (.vec _) => none
  | .res .backend => some false
  | .res .unavailable => some true
  | .dropped => some false
  | .timedOut => some true

/-- Where the worker thread is. `down`: no worker running (before the
supervisor's first spawn, or after a death); `idle`: in `wait_for_batch`;
`debounce`: holding a take, before its top-up; `serving`: at the head of
`serve_take`'s loop; `sending`: a forward done, its answers not all sent;
`exited`: returned after `drain_all`. -/
inductive Pc where
  | down
  | idle
  | debounce (batch : List Req)
  | serving (t : Take)
  | sending (t : Take) (out : List (Req × Res))
  | exited
  deriving DecidableEq, Repr

/-- The requests the worker holds outside the queue. -/
def Pc.hand : Pc → List Req
  | .debounce b => b
  | .serving t => t.items
  | .sending t out => t.items ++ out.map Prod.fst
  | _ => []

structure St where
  queue : List Req
  /-- `BatchQueue::shutdown`. -/
  shutdown : Bool
  pc : Pc
  /-- The supervisor thread is running. -/
  sup : Bool
  /-- `Shared` is alive (some `Arc<Shared>` remains). -/
  live : Bool
  /-- Every request ever queued. -/
  issued : List Req
  /-- Requests whose caller stopped waiting (receiver dropped). -/
  gaveUp : List Req
  /-- What each caller observed, in order. -/
  obs : List (Req × Obs)
  /-- Requests whose `Pending` is gone (sent, discarded or dropped). -/
  done : List Req
  /-- Forward calls so far. -/
  calls : Nat
  /-- Every forward the device ran, in order. -/
  fwds : List (List Txt)
  deriving DecidableEq, Repr

def St.init : St := ⟨[], false, .down, true, true, [], [], [], [], 0, []⟩

/-- A request's `Pending` goes away with outcome `o`; its caller observes `o`
unless it already gave up. -/
def St.settle (s : St) (r : Req) (o : Obs) : St :=
  { s with
    done := s.done ++ [r]
    obs := if r ∈ s.gaveUp then s.obs else s.obs ++ [(r, o)] }

def settleAll (s : St) (rs : List Req) (o : Obs) : St := rs.foldl (fun s r => s.settle r o) s

/-- One step of the worker thread (`coalesce_loop` and `serve_take`), with
the forward `f` and the contract width `dim`. Deterministic; a state with
nothing to do is left as it is (blocked on the condvar, or not running). -/
def work (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : St :=
  match s.pc with
  | .idle =>
    if s.queue ≠ [] then
      let r := takePrioritized s.queue c.maxBatch
      if r.1.length < c.maxBatch then { s with queue := r.2, pc := .debounce r.1 }
      else { s with queue := r.2, pc := .serving (split r.1) }
    else if s.shutdown then
      -- `wait_for_batch` returned `None`; `drain_all` fails what raced in.
      { settleAll { s with queue := [] } s.queue (.res .unavailable) with pc := .exited }
    else s
  | .debounce b =>
    let r := takePrioritized s.queue (c.maxBatch - b.length)
    { s with queue := r.2, pc := .serving (split (b ++ r.1)) }
  | .serving t =>
    let p := pullOf c s.shutdown s.queue t
    match pick c p.1 t with
    | none => { s with queue := p.2, pc := .idle }
    | some (items, t') =>
      let awaited := items.filter (fun r => r ∉ s.gaveUp)
      let s1 := settleAll { s with queue := p.2 } (items.filter (fun r => r ∈ s.gaveUp)) .dropped
      if awaited = [] then { s1 with pc := .serving t' }
      else
        let fi := forwardIsolated f dim s.calls (awaited.map (·.txt))
        { s1 with
          pc := .sending t' (awaited.zip fi.1)
          calls := s.calls + fi.2.length
          fwds := s.fwds ++ fi.2 }
  | .sending t out =>
    match out with
    | [] => { s with pc := .serving t }
    | [o] => { s.settle o.1 (.res o.2) with pc := .serving t }
    | o :: os => { s.settle o.1 (.res o.2) with pc := .sending t os }
  | _ => s

/-- The events: callers, the last handle's drop, the worker, its death, the
supervisor, and the drop of `Shared`. -/
inductive Ev where
  /-- `embed`: push one single request. -/
  | enqueue (r : Req)
  /-- `embed_batch`: `push_many` of bulk requests under one lock. -/
  | enqueueMany (rs : List Req)
  /-- A caller's deadline passes and it drops its receiver. -/
  | giveUp (r : Req)
  /-- The last adapter handle drops: `Handle::drop` → `request_shutdown`. -/
  | shutdown
  /-- One step of the worker thread. -/
  | work
  /-- The worker thread dies (a panic outside `catch_unwind`). -/
  | die
  /-- The supervisor reacts to a worker that is not running: exit on
  shutdown, otherwise spawn a new one (`spawnOk = false`: the spawn's
  `expect` panics and the supervisor dies). -/
  | supervise (spawnOk : Bool)
  /-- The last `Arc<Shared>` drops, and the queue with it. -/
  | dropShared
  deriving DecidableEq, Repr

def Pc.running : Pc → Bool
  | .idle => true
  | .debounce _ => true
  | .serving _ => true
  | .sending _ _ => true
  | _ => false

def step (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : Ev → St
  | .enqueue r =>
    if s.live ∧ s.shutdown = false ∧ r.bulk = false ∧ r ∉ s.issued then
      { s with queue := s.queue ++ [r], issued := s.issued ++ [r] }
    else s
  | .enqueueMany rs =>
    if s.live ∧ s.shutdown = false ∧ rs.all (·.bulk) ∧ rs.Nodup ∧ (∀ r ∈ rs, r ∉ s.issued) then
      { s with queue := s.queue ++ rs, issued := s.issued ++ rs }
    else s
  | .giveUp r =>
    if r ∈ s.issued ∧ r ∉ s.obs.map Prod.fst then
      { s with gaveUp := s.gaveUp ++ [r], obs := s.obs ++ [(r, .timedOut)] }
    else s
  | .shutdown => { s with shutdown := true }
  | .work => work c f dim s
  | .die =>
    if s.pc.running then { settleAll s s.pc.hand .dropped with pc := .down } else s
  | .supervise ok =>
    if s.sup ∧ (s.pc = .down ∨ s.pc = .exited) then
      if s.shutdown then { s with sup := false }
      else if s.pc = .down then (if ok then { s with pc := .idle } else { s with sup := false })
      else s
    else s
  | .dropShared =>
    if s.live ∧ s.shutdown ∧ s.sup = false ∧ (s.pc = .down ∨ s.pc = .exited) then
      { settleAll { s with queue := [] } s.queue .dropped with live := false }
    else s

def run (c : Cfg) (f : Fwd) (dim : Nat) (s : St) (evs : List Ev) : St :=
  evs.foldl (step c f dim) s

/-! ## Invariants of the state machine -/

theorem settleAll_eq (s : St) (rs : List Req) (o : Obs) :
    settleAll s rs o =
      { s with
        done := s.done ++ rs
        obs := s.obs ++ (rs.filter (fun r => r ∉ s.gaveUp)).map (fun r => (r, o)) } := by
  induction rs generalizing s with
  | nil => simp [settleAll]
  | cons r rs ih =>
    show settleAll (s.settle r o) rs o = _
    rw [ih]
    by_cases h : r ∈ s.gaveUp
    · simp [St.settle, h]
    · simp [St.settle, h]

/-- The bookkeeping every reachable state keeps. `cons`: every request ever
queued is in exactly one place: queued, in the worker's hands, or gone.
`obsUniq`: no caller observes twice. `doneObs`: a request that is gone has
been observed (answered, dropped, or timed out before). -/
structure Core (s : St) : Prop where
  cons : ∀ r, s.issued.count r = s.queue.count r + s.pc.hand.count r + s.done.count r
  uniq : ∀ r, s.issued.count r ≤ 1
  obsUniq : ∀ r, (s.obs.map Prod.fst).count r ≤ 1
  gave : ∀ r ∈ s.gaveUp, r ∈ s.obs.map Prod.fst
  doneObs : ∀ r ∈ s.done, r ∈ s.obs.map Prod.fst
  obsWhy : ∀ r ∈ s.obs.map Prod.fst, r ∈ s.gaveUp ∨ r ∈ s.done

theorem map_fst_pair (rs : List Req) (o : Obs) : (rs.map (fun r => (r, o))).map Prod.fst = rs := by
  induction rs with
  | nil => rfl
  | cons r rs ih => simp only [List.map_cons, ih]

theorem core_transfer {s s' : St} {rs : List Req} {o : Obs} (hc : Core s)
    (hi : s'.issued = s.issued) (hg : s'.gaveUp = s.gaveUp) (hd : s'.done = s.done ++ rs)
    (ho : s'.obs = s.obs ++ (rs.filter (fun r => r ∉ s.gaveUp)).map (fun r => (r, o)))
    (hcnt : ∀ r, s'.queue.count r + s'.pc.hand.count r + rs.count r =
      s.queue.count r + s.pc.hand.count r) : Core s' := by
  have hobs : s'.obs.map Prod.fst = s.obs.map Prod.fst ++ rs.filter (fun r => r ∉ s.gaveUp) := by
    rw [ho, List.map_append, map_fst_pair]
  constructor
  · intro r; rw [hi, hd, List.count_append]; have := hc.cons r; have := hcnt r; omega
  · intro r; rw [hi]; exact hc.uniq r
  · intro r
    rw [hobs, List.count_append]
    by_cases hr : r ∈ s.gaveUp
    · have : (rs.filter (fun r => r ∉ s.gaveUp)).count r = 0 :=
        List.count_eq_zero.mpr (by simp [hr])
      have := hc.obsUniq r; omega
    · have hf : (rs.filter (fun r => r ∉ s.gaveUp)).count r = rs.count r :=
        List.count_filter (by simp [hr])
      rw [hf]
      by_cases h0 : rs.count r = 0
      · have := hc.obsUniq r; omega
      · have hc1 := hc.cons r; have hu := hc.uniq r; have hcr := hcnt r
        have hd0 : s.done.count r = 0 := by omega
        have hnd : r ∉ s.done := List.count_eq_zero.mp hd0
        have ho0 : (s.obs.map Prod.fst).count r = 0 := by
          apply List.count_eq_zero.mpr
          intro hm
          rcases hc.obsWhy r hm with h | h
          · exact hr h
          · exact hnd h
        omega
  · intro r hr; rw [hg] at hr; rw [hobs]; exact List.mem_append_left _ (hc.gave r hr)
  · intro r hr
    rw [hd] at hr; rw [hobs]
    rcases List.mem_append.mp hr with hr | hr
    · exact List.mem_append_left _ (hc.doneObs r hr)
    · by_cases hg' : r ∈ s.gaveUp
      · exact List.mem_append_left _ (hc.gave r hg')
      · exact List.mem_append_right _ (List.mem_filter.mpr ⟨hr, by simp [hg']⟩)
  · intro r hr
    rw [hobs] at hr; rw [hg, hd]
    rcases List.mem_append.mp hr with hr | hr
    · rcases hc.obsWhy r hr with h | h
      · exact Or.inl h
      · exact Or.inr (List.mem_append_left _ h)
    · exact Or.inr (List.mem_append_right _ (List.mem_filter.mp hr).1)

theorem core_keep {s s' : St} (hc : Core s) (hi : s'.issued = s.issued) (hg : s'.gaveUp = s.gaveUp)
    (hd : s'.done = s.done) (ho : s'.obs = s.obs)
    (hcnt : ∀ r, s'.queue.count r + s'.pc.hand.count r = s.queue.count r + s.pc.hand.count r) :
    Core s' :=
  core_transfer (rs := []) (o := .dropped) hc hi hg (by simp [hd]) (by simp [ho])
    (by intro r; simpa using hcnt r)

/-- Every request is answered at most once, and once nothing is queued or in
the worker's hands, every request ever queued has been answered exactly
once. -/
theorem core_at_rest {s : St} (hc : Core s) (hq : s.queue = []) (hh : s.pc.hand = []) :
    ∀ r ∈ s.issued, (s.obs.map Prod.fst).count r = 1 := by
  intro r hr
  have h1 := hc.cons r
  have hpos : 0 < s.issued.count r := List.count_pos_iff.mpr hr
  rw [hq, hh] at h1
  simp only [List.count_nil, Nat.zero_add] at h1
  have hd : r ∈ s.done := List.count_pos_iff.mp (by omega)
  have := List.count_pos_iff.mpr (hc.doneObs r hd)
  have := hc.obsUniq r
  omega


/-- `Core`, plus the shape facts the theorems below read: an exited worker
and a dropped `Shared` leave nothing queued; the caps on takes and forwards;
the forward counter; and `wired`: the answers being sent are, position by
position, `forward_isolated`'s results on the texts of the requests they go
to (no cross-wiring). -/
structure Good (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : Prop extends Core s where
  exitedQ : s.pc = .exited → s.queue = [] ∧ s.shutdown = true
  deadQ : s.live = false → s.queue = [] ∧ s.shutdown = true ∧ (s.pc = .down ∨ s.pc = .exited)
  debCap : ∀ b, s.pc = .debounce b → b.length ≤ c.maxBatch
  sendCap : ∀ t out, s.pc = .sending t out → out.length ≤ c.width
  fwdCaps : ∀ b ∈ s.fwds, b.length ≤ c.width ∧ Slack c b
  calls : s.calls = s.fwds.length
  wired : ∀ t out, s.pc = .sending t out →
    ∃ pre k, (pre ++ out).map Prod.snd = (forwardIsolated f dim k ((pre ++ out).map (·.1.txt))).1

theorem good_init (c : Cfg) (f : Fwd) (dim : Nat) : Good c f dim St.init where
  cons := by simp [St.init, Pc.hand]
  uniq := by simp [St.init]
  obsUniq := by simp [St.init]
  gave := by simp [St.init]
  doneObs := by simp [St.init]
  obsWhy := by simp [St.init]
  exitedQ := by simp [St.init]
  deadQ := by simp [St.init]
  debCap := by simp [St.init]
  sendCap := by simp [St.init]
  fwdCaps := by simp [St.init]
  calls := rfl
  wired := by simp [St.init]

theorem pullOf_perm (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    ((pullOf c sd q t).1 ++ (pullOf c sd q t).2).Perm q := by
  unfold pullOf; split
  · exact takeSingles_perm _ _
  · exact List.Perm.refl _

theorem pullOf_nil (c : Cfg) (sd : Bool) (q : List Req) (t : Take) (h : t.bulk = [] ∨ sd = true) :
    pullOf c sd q t = ([], q) := by
  unfold pullOf
  have : ¬(t.bulk ≠ [] ∧ sd = false) := by
    rintro ⟨h1, h2⟩
    rcases h with h | h
    · exact h1 h
    · rw [h] at h2; cases h2
  simp only [this, ite_false]

theorem pullOf_length (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    (pullOf c sd q t).1.length ≤ c.maxBatch := by
  unfold pullOf; split
  · rw [takeSingles_fst]; simp only [List.length_take]; exact Nat.min_le_left _ _
  · simp

theorem split_count (b : List Req) (r : Req) : (split b).items.count r = b.count r := by
  have := perm_count (List.filter_append_perm (fun p : Req => !p.bulk) b) r
  simp only [Bool.not_not] at this
  simp only [split, Take.items]
  exact this

theorem settle_eq (s : St) (r : Req) (o : Obs) : s.settle r o = settleAll s [r] o := rfl

section work
variable {c : Cfg} {f : Fwd} {dim : Nat} {s : St}

theorem good_work_idle (hg : Good c f dim s) (hpc : s.pc = .idle) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  split
  · next hq =>
    have hp := takePrioritized_perm s.queue c.maxBatch
    have hl := takePrioritized_length_le s.queue c.maxBatch
    split
    · exact {
        toCore := core_keep hg.toCore rfl rfl rfl rfl (by
          intro r; have := perm_count hp r
          simp only [List.count_append, Pc.hand, hpc, List.count_nil] at this ⊢; omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by intro b hb; simp only [Pc.debounce.injEq] at hb; subst hb; exact hl
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · exact {
        toCore := core_keep hg.toCore rfl rfl rfl rfl (by
          intro r; have := perm_count hp r
          simp only [List.count_append, Pc.hand, hpc, split_count, List.count_nil] at this ⊢; omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
  · next hq =>
    have hq' : s.queue = [] := by simpa using hq
    split
    · next hsd =>
      rw [hq', settleAll_eq]
      exact {
        toCore := core_keep hg.toCore rfl rfl (by simp) (by simp) (by
          intro r; simp [hq', Pc.hand, hpc])
        exitedQ := fun _ => ⟨rfl, by simpa using hsd⟩
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · exact hg

theorem good_work_debounce (hg : Good c f dim s) (b : List Req) (hpc : s.pc = .debounce b) :
    Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  have hp := takePrioritized_perm s.queue (c.maxBatch - b.length)
  exact {
    toCore := core_keep hg.toCore rfl rfl rfl rfl (by
      intro r; have := perm_count hp r
      simp only [List.count_append, Pc.hand, hpc, split_count] at this ⊢; omega)
    exitedQ := by simp
    deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
    debCap := by simp
    sendCap := by simp
    fwdCaps := hg.fwdCaps
    calls := hg.calls
    wired := by simp }

theorem good_work_sending (hg : Good c f dim s) (t : Take) (out : List (Req × Res))
    (hpc : s.pc = .sending t out) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  obtain ⟨pre, k, hw⟩ := hg.wired t out hpc
  have hsc := hg.sendCap t out hpc
  split
  · exact {
      toCore := core_keep hg.toCore rfl rfl rfl rfl (by intro r; simp [Pc.hand, hpc])
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next o =>
    rw [settle_eq, settleAll_eq]
    exact {
      toCore := core_transfer (rs := [o.1]) (o := .res o.2) hg.toCore rfl rfl rfl rfl (by
        intro r; simp only [Pc.hand, hpc, List.count_append, List.map_cons, List.count_cons,
          List.count_nil, List.map_nil]; omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next o os hos =>
    rw [settle_eq, settleAll_eq]
    exact {
      toCore := core_transfer (rs := [o.1]) (o := .res o.2) hg.toCore rfl rfl rfl rfl (by
        intro r; simp only [Pc.hand, hpc, List.count_append, List.map_cons, List.count_cons,
          List.count_nil]; omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by
        intro t' out' h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
        simp at hsc; omega
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by
        intro t' out' h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
        exact ⟨pre ++ [o], k, by simpa using hw⟩ }

theorem count_filter_split (g : List Req) (items : List Req) (r : Req) :
    (items.filter (fun x => x ∉ g)).count r + (items.filter (fun x => x ∈ g)).count r =
      items.count r := by
  have h := perm_count (List.filter_append_perm (fun x : Req => decide (x ∉ g)) items) r
  have he : (fun x : Req => !decide (x ∉ g)) = (fun x => decide (x ∈ g)) := by
    funext x; simp
  rw [List.count_append, he] at h
  exact h

theorem map_txt_of_fst (out : List (Req × Res)) (aw : List Req) (h : out.map Prod.fst = aw) :
    out.map (fun x => x.1.txt) = aw.map (·.txt) := by
  rw [← h, List.map_map]; rfl

theorem good_work_serving (hw : 1 ≤ c.width) (hg : Good c f dim s) (t : Take)
    (hpc : s.pc = .serving t) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  have hpp := pullOf_perm c s.shutdown s.queue t
  split
  · next hn =>
    obtain ⟨hb, hs⟩ := (pick_none_iff c _ t).mp hn
    have hs1 : t.single = [] := (List.append_eq_nil_iff.mp hs).1
    have hp1 : (pullOf c s.shutdown s.queue t).1 = [] := (List.append_eq_nil_iff.mp hs).2
    exact {
      toCore := core_keep hg.toCore rfl rfl rfl rfl (by
        intro r; have := perm_count hpp r
        simp only [List.count_append, hp1, List.count_nil, Pc.hand, hpc, Take.items, hb, hs1,
          List.append_nil] at this ⊢
        omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next items t' hpick =>
    have hperm := pick_perm c _ t items t' hpick
    have hcaps := pick_items_caps c _ t items t' hw hpick
    have hsplit := count_filter_split s.gaveUp items
    split
    · next haw =>
      rw [settleAll_eq]
      exact {
        toCore := core_transfer (rs := items.filter (fun r => r ∈ s.gaveUp)) (o := .dropped)
          hg.toCore rfl rfl rfl rfl (by
            intro r
            have h1 := perm_count hperm r; have h2 := perm_count hpp r; have h3 := hsplit r
            rw [haw] at h3
            simp only [List.count_append, List.count_nil, Pc.hand, hpc] at h1 h2 h3 ⊢
            omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · next haw =>
      have hne : (items.filter (fun r => r ∉ s.gaveUp)).map (·.txt) ≠ [] := by simpa using haw
      have hlen := forwardIsolated_length f dim s.calls _ hne
      simp only [List.length_map] at hlen
      have hfst : ((items.filter (fun r => r ∉ s.gaveUp)).zip
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1).map
            Prod.fst = items.filter (fun r => r ∉ s.gaveUp) := List.map_fst_zip (by omega)
      have hsnd : ((items.filter (fun r => r ∉ s.gaveUp)).zip
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1).map
            Prod.snd =
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1 :=
        List.map_snd_zip (by omega)
      have hawl : (items.filter (fun r => r ∉ s.gaveUp)).length ≤ c.width :=
        Nat.le_trans (List.length_filter_le _ _) hcaps.1
      rw [settleAll_eq]
      exact {
        toCore := core_transfer (rs := items.filter (fun r => r ∈ s.gaveUp)) (o := .dropped)
          hg.toCore rfl rfl rfl rfl (by
            intro r
            have h1 := perm_count hperm r; have h2 := perm_count hpp r; have h3 := hsplit r
            simp only [Pc.hand, hfst, List.count_append, Pc.hand, hpc] at h1 h2 h3 ⊢
            omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by
          intro t2 out h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
          simp only [List.length_zip]; omega
        fwdCaps := by
          intro b hb
          rcases List.mem_append.mp hb with hb | hb
          · exact hg.fwdCaps b hb
          · refine forwardIsolated_fwds_caps c f dim s.calls _ (by simpa using hawl) hw ?_ b hb
            exact hcaps.2.sublist ((List.filter_sublist).map _)
        calls := by simp [hg.calls]
        wired := by
          intro t2 out h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
          refine ⟨[], s.calls, ?_⟩
          simp only [List.nil_append]
          rw [hsnd, map_txt_of_fst _ _ hfst] }

theorem good_work (hw : 1 ≤ c.width) (hg : Good c f dim s) : Good c f dim (work c f dim s) := by
  cases hpc : s.pc with
  | idle => exact good_work_idle hg hpc
  | debounce b => exact good_work_debounce hg b hpc
  | serving t => exact good_work_serving hw hg t hpc
  | sending t out => exact good_work_sending hg t out hpc
  | down => simp only [work, hpc]; exact hg
  | exited => simp only [work, hpc]; exact hg

end work


section step
variable {c : Cfg} {f : Fwd} {dim : Nat} {s : St}

theorem count_single_le (r x : Req) : [r].count x ≤ 1 := by
  rw [List.count_singleton]; split <;> omega

theorem count_single_of_ne {r x : Req} (h : x ≠ r) : [r].count x = 0 :=
  List.count_eq_zero.mpr (by simpa using h)

theorem good_enqueue (hg : Good c f dim s) (r : Req) : Good c f dim (step c f dim s (.enqueue r)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨hl, hsd, -, hfresh⟩ := h
    exact {
      cons := by
        intro x; have := hg.cons x; simp only [List.count_append]; omega
      uniq := by
        intro x; simp only [List.count_append]
        by_cases hx : x = r
        · subst hx; rw [List.count_eq_zero.mpr hfresh]; exact Nat.le_trans (by omega) (count_single_le x x)
        · rw [count_single_of_ne hx]; exact hg.uniq x
      obsUniq := hg.obsUniq
      gave := hg.gave
      doneObs := hg.doneObs
      obsWhy := hg.obsWhy
      exitedQ := by intro he; have := (hg.exitedQ he).2; simp_all
      deadQ := by intro hd; simp_all
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_enqueueMany (hg : Good c f dim s) (rs : List Req) :
    Good c f dim (step c f dim s (.enqueueMany rs)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨hl, hsd, -, hnd, hfresh⟩ := h
    exact {
      cons := by
        intro x; have := hg.cons x; simp only [List.count_append]; omega
      uniq := by
        intro x; simp only [List.count_append]
        rw [List.Nodup.count hnd]
        by_cases hx : x ∈ rs
        · simp only [hx, ite_true, List.count_eq_zero.mpr (hfresh x hx)]; omega
        · simp only [hx, ite_false]; have := hg.uniq x; omega
      obsUniq := hg.obsUniq
      gave := hg.gave
      doneObs := hg.doneObs
      obsWhy := hg.obsWhy
      exitedQ := by intro he; have := (hg.exitedQ he).2; simp_all
      deadQ := by intro hd; simp_all
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_giveUp (hg : Good c f dim s) (r : Req) : Good c f dim (step c f dim s (.giveUp r)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨-, hno⟩ := h
    have hobs : ((s.obs ++ [(r, Obs.timedOut)]).map Prod.fst) = s.obs.map Prod.fst ++ [r] := by simp
    exact {
      cons := hg.cons
      uniq := hg.uniq
      obsUniq := by
        intro x; simp only [hobs, List.count_append]
        by_cases hx : x = r
        · subst hx; rw [List.count_eq_zero.mpr hno]; exact Nat.le_trans (by omega) (count_single_le x x)
        · rw [count_single_of_ne hx]; exact hg.obsUniq x
      gave := by
        intro x hx; simp only [hobs]
        rcases List.mem_append.mp hx with hx | hx
        · exact List.mem_append_left _ (hg.gave x hx)
        · exact List.mem_append_right _ hx
      doneObs := by intro x hx; simp only [hobs]; exact List.mem_append_left _ (hg.doneObs x hx)
      obsWhy := by
        intro x hx; simp only [hobs] at hx
        rcases List.mem_append.mp hx with hx | hx
        · rcases hg.obsWhy x hx with h | h
          · exact Or.inl (List.mem_append_left _ h)
          · exact Or.inr h
        · exact Or.inl (List.mem_append_right _ hx)
      exitedQ := hg.exitedQ
      deadQ := hg.deadQ
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_shutdown (hg : Good c f dim s) : Good c f dim (step c f dim s .shutdown) :=
  { hg.toCore with
    exitedQ := fun h => ⟨(hg.exitedQ h).1, rfl⟩
    deadQ := fun h => ⟨(hg.deadQ h).1, rfl, (hg.deadQ h).2.2⟩
    debCap := hg.debCap
    sendCap := hg.sendCap
    fwdCaps := hg.fwdCaps
    calls := hg.calls
    wired := hg.wired }

theorem good_die (hg : Good c f dim s) : Good c f dim (step c f dim s .die) := by
  simp only [step]
  split
  · next hr =>
    rw [settleAll_eq]
    exact {
      toCore := core_transfer (rs := s.pc.hand) (o := .dropped) hg.toCore rfl rfl rfl rfl (by
        intro x; simp [Pc.hand])
      exitedQ := by simp
      deadQ := by
        intro hd; have := (hg.deadQ hd).2.2
        rcases this with h | h <;> rw [h] at hr <;> simp [Pc.running] at hr
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · exact hg

theorem good_supervise (hg : Good c f dim s) (ok : Bool) :
    Good c f dim (step c f dim s (.supervise ok)) := by
  simp only [step]
  split
  · next h =>
    split
    · exact { hg with }
    · next hsd =>
      split
      · next hdown =>
        split
        · exact {
            toCore := core_keep hg.toCore rfl rfl rfl rfl (by intro x; simp [Pc.hand, hdown])
            exitedQ := by simp
            deadQ := by intro hd; have := (hg.deadQ hd).2.1; simp_all
            debCap := by simp
            sendCap := by simp
            fwdCaps := hg.fwdCaps
            calls := hg.calls
            wired := by simp }
        · exact { hg with }
      · exact hg
  · exact hg

theorem good_dropShared (hg : Good c f dim s) : Good c f dim (step c f dim s .dropShared) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨-, hsd, -, hpc⟩ := h
    have hh : s.pc.hand = [] := by rcases hpc with h | h <;> rw [h] <;> rfl
    rw [settleAll_eq]
    exact {
      toCore := core_transfer (rs := s.queue) (o := .dropped) hg.toCore rfl rfl rfl rfl (by
        intro x; simp [hh])
      exitedQ := fun _ => ⟨rfl, hsd⟩
      deadQ := fun _ => ⟨rfl, hsd, hpc⟩
      debCap := by rcases hpc with h | h <;> simp [h]
      sendCap := by rcases hpc with h | h <;> simp [h]
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by rcases hpc with h | h <;> simp [h] }
  · exact hg

/-- Every reachable state keeps the invariant. -/
theorem good_step (hw : 1 ≤ c.width) (hg : Good c f dim s) (e : Ev) : Good c f dim (step c f dim s e) := by
  cases e with
  | enqueue r => exact good_enqueue hg r
  | enqueueMany rs => exact good_enqueueMany hg rs
  | giveUp r => exact good_giveUp hg r
  | shutdown => exact good_shutdown hg
  | work => exact good_work hw hg
  | die => exact good_die hg
  | supervise ok => exact good_supervise hg ok
  | dropShared => exact good_dropShared hg

end step

theorem good_run {c : Cfg} {f : Fwd} {dim : Nat} (hw : 1 ≤ c.width) (s : St) (hg : Good c f dim s)
    (evs : List Ev) : Good c f dim (run c f dim s evs) := by
  induction evs generalizing s with
  | nil => exact hg
  | cons e es ih => exact ih _ (good_step hw hg e)

theorem good_reachable {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev) :
    Good c f dim (run c f dim St.init evs) :=
  good_run hw _ (good_init c f dim) evs


/-! ## Liveness: the worker drains -/

def Pc.rank : Pc → Nat
  | .idle => 3
  | .debounce b => if b = [] then 5 else 2
  | .serving t => if t.items = [] then 4 else 1
  | .sending _ out => if out = [] then 5 else 0
  | .down => 0
  | .exited => 0

/-- Six per request queued or held, plus where the worker is. -/
def measure (s : St) : Nat := 6 * (s.queue.length + s.pc.hand.length) + s.pc.rank

/-- The worker has something to do: it is mid-take, or idle with work queued
or shutdown requested. -/
def Busy (s : St) : Prop :=
  match s.pc with
  | .idle => s.queue ≠ [] ∨ s.shutdown = true
  | .debounce _ => True
  | .serving _ => True
  | .sending _ _ => True
  | _ => False

theorem work_not_busy {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (h : ¬Busy s) : work c f dim s = s := by
  unfold Busy at h
  unfold work
  split
  · next hpc =>
    rw [hpc] at h
    simp only [not_or, Decidable.not_not] at h
    simp [h.1, h.2]
  all_goals first | rfl | (next hpc => rw [hpc] at h; simp at h)

theorem length_count_split (b : List Req) : (split b).items.length = b.length := by
  have := (List.filter_append_perm (fun p : Req => !p.bulk) b).length_eq
  simp only [Bool.not_not] at this
  simpa [split, Take.items] using this

/-- Every step of a busy worker lowers `measure`; only a new request raises
it. With `MAX_BATCH > 0`. -/
theorem work_progress {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (hm : 0 < c.maxBatch) (h : Busy s) :
    measure (work c f dim s) < measure s := by
  unfold Busy at h
  unfold work measure
  cases hpc : s.pc with
  | down => rw [hpc] at h; exact absurd h id
  | exited => rw [hpc] at h; exact absurd h id
  | idle =>
    simp only
    split
    · next hq =>
      have hl := (takePrioritized_perm s.queue c.maxBatch).length_eq
      have hlen := takePrioritized_length s.queue c.maxBatch
      have hq0 : 0 < s.queue.length := List.length_pos_iff.mpr hq
      have hne : (takePrioritized s.queue c.maxBatch).1 ≠ [] := by
        intro h0; rw [h0] at hlen; simp at hlen; omega
      simp only [List.length_append] at hl
      split
      · simp only [Pc.hand, Pc.rank, hne, ite_false]; omega
      · have hs : (split (takePrioritized s.queue c.maxBatch).1).items ≠ [] := by
          intro h0; have := length_count_split (takePrioritized s.queue c.maxBatch).1
          rw [h0] at this; simp at this; exact hne (List.length_eq_zero_iff.mp this.symm)
        simp only [Pc.hand, Pc.rank, hs, ite_false, length_count_split]; omega
    · split
      · next hq _ =>
        have hq' : s.queue = [] := by simpa using hq
        rw [hq', settleAll_eq]; simp [Pc.hand, Pc.rank]
      · next hq hsd => rw [hpc] at h; simp at hq hsd; simp_all
  | debounce b =>
    simp only
    have hl := (takePrioritized_perm s.queue (c.maxBatch - b.length)).length_eq
    simp only [List.length_append] at hl
    have hlen := length_count_split (b ++ (takePrioritized s.queue (c.maxBatch - b.length)).1)
    simp only [List.length_append] at hlen
    by_cases hb : b = []
    · simp only [Pc.hand, Pc.rank, hb, ite_true]; split <;> simp_all <;> omega
    · have hs : (split (b ++ (takePrioritized s.queue (c.maxBatch - b.length)).1)).items ≠ [] := by
        intro h0; rw [h0] at hlen; simp at hlen; exact hb (List.length_eq_zero_iff.mp (by omega))
      simp only [Pc.hand, Pc.rank, hb, hs, ite_false, hlen]; omega
  | sending t out =>
    simp only
    split
    · simp only [Pc.hand, Pc.rank, List.map_nil, List.append_nil, ite_true]; split <;> omega
    · rw [settle_eq, settleAll_eq]; simp [Pc.hand, Pc.rank]; split <;> omega
    · next o os hos =>
      rw [settle_eq, settleAll_eq]
      simp only [Pc.hand, Pc.rank, List.map_cons, List.length_append, List.length_cons, List.length_map]
      have : os ≠ [] := hos
      simp [this]
  | serving t =>
    simp only
    have hpp := (pullOf_perm c s.shutdown s.queue t).length_eq
    simp only [List.length_append] at hpp
    split
    · next hn =>
      obtain ⟨hb, hs⟩ := (pick_none_iff c _ t).mp hn
      have hti : t.items = [] := by
        simp [Take.items, hb, (List.append_eq_nil_iff.mp hs).1]
      have hp1 : (pullOf c s.shutdown s.queue t).1 = [] := (List.append_eq_nil_iff.mp hs).2
      simp only [Pc.hand, Pc.rank, hti, ite_true, hp1, List.length_nil] at hpp ⊢
      omega
    · next items t' hpick =>
      have hperm := (pick_perm c _ t items t' hpick).length_eq
      simp only [List.length_append] at hperm
      have hne := List.length_pos_iff.mpr (pick_items_ne c _ t items t' hpick)
      have hti : t.items ≠ [] := by
        intro h0
        have hb : t.bulk = [] := (List.append_eq_nil_iff.mp h0).2
        have hs : t.single = [] := (List.append_eq_nil_iff.mp h0).1
        have hp : (pullOf c s.shutdown s.queue t).1 = [] := by rw [pullOf_nil c _ _ t (Or.inl hb)]
        rw [hp] at hpick
        have := (pick_none_iff c [] t).mpr ⟨hb, by simp [hs]⟩
        rw [this] at hpick; cases hpick
      have hsp := (List.filter_append_perm (fun x : Req => decide (x ∉ s.gaveUp)) items).length_eq
      simp only [List.length_append] at hsp
      have he : (fun x : Req => !decide (x ∉ s.gaveUp)) = (fun x => decide (x ∈ s.gaveUp)) := by
        funext x; simp
      rw [he] at hsp
      split
      · next haw =>
        rw [settleAll_eq]
        simp only [Pc.hand, Pc.rank, hti, ite_false, haw, List.length_nil] at hsp ⊢
        split <;> omega
      · next haw =>
        have hne' : (items.filter (fun r => r ∉ s.gaveUp)).map (·.txt) ≠ [] := by simpa using haw
        have hlen := forwardIsolated_length f dim s.calls _ hne'
        simp only [List.length_map] at hlen
        have hfst := List.map_fst_zip (l₁ := items.filter (fun r => r ∉ s.gaveUp))
          (l₂ := (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1)
          (by omega)
        have hz : (items.filter (fun r => r ∉ s.gaveUp)).zip
            (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1 ≠ [] := by
          intro h0; rw [h0] at hfst; exact haw hfst.symm
        rw [settleAll_eq]
        simp only [Pc.hand, Pc.rank, hti, hz, ite_false, hfst, List.length_append]
        omega

def workN (c : Cfg) (f : Fwd) (dim : Nat) : Nat → St → St
  | 0, s => s
  | n + 1, s => workN c f dim n (work c f dim s)

theorem workN_eq_run (c : Cfg) (f : Fwd) (dim : Nat) (n : Nat) (s : St) :
    workN c f dim n s = run c f dim s (List.replicate n .work) := by
  induction n generalizing s with
  | zero => rfl
  | succ n ih => simp [workN, ih, run, List.replicate_succ, step]

/-- Left alone (no new requests), the worker stops being busy within
`measure s` steps. -/
theorem workN_idle (c : Cfg) (f : Fwd) (dim : Nat) (hm : 0 < c.maxBatch) :
    ∀ n s, measure s ≤ n → ¬Busy (workN c f dim n s) := by
  intro n
  induction n with
  | zero =>
    intro s hs hb
    simp only [workN] at hb
    have := work_progress (f := f) (dim := dim) hm hb
    omega
  | succ n ih =>
    intro s hs
    by_cases hb : Busy s
    · have := work_progress (f := f) (dim := dim) hm hb
      exact ih _ (by omega)
    · simp only [workN, work_not_busy hb]
      by_cases hn : measure s ≤ n
      · exact ih s hn
      · -- not busy: the worker is a fixed point
        have : ∀ k, workN c f dim k s = s := by
          intro k; induction k with
          | zero => rfl
          | succ k ihk => simp [workN, work_not_busy hb, ihk]
        rw [this]; exact hb

theorem work_pc_ne_down {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (h : s.pc ≠ .down) :
    (work c f dim s).pc ≠ .down := by
  unfold work
  cases hpc : s.pc with
  | down => exact absurd hpc h
  | exited => simp [hpc]
  | idle =>
    simp only
    split
    · split <;> simp
    · split
      · rw [settleAll_eq]; simp
      · simp [hpc]
  | debounce b => simp
  | serving t =>
    simp only
    split
    · simp
    · split
      · rw [settleAll_eq]; simp
      · rw [settleAll_eq]; simp
  | sending t out =>
    simp only
    split
    · simp
    · rw [settle_eq, settleAll_eq]; simp
    · rw [settle_eq, settleAll_eq]; simp

theorem workN_pc_ne_down {c : Cfg} {f : Fwd} {dim : Nat} (n : Nat) {s : St} (h : s.pc ≠ .down) :
    (workN c f dim n s).pc ≠ .down := by
  induction n generalizing s with
  | zero => exact h
  | succ n ih => exact ih (work_pc_ne_down h)

/-- The worker touches only the queue, its own state, the answers and the
forward log. -/
theorem work_frame {c : Cfg} {f : Fwd} {dim : Nat} (s : St) :
    (work c f dim s).issued = s.issued ∧ (work c f dim s).shutdown = s.shutdown ∧
      (work c f dim s).gaveUp = s.gaveUp ∧ (work c f dim s).live = s.live ∧
      (work c f dim s).sup = s.sup := by
  unfold work
  split <;> (try dsimp only) <;> (repeat' split) <;> simp [settleAll_eq, settle_eq]

theorem work_shutdown {c : Cfg} {f : Fwd} {dim : Nat} {s : St} :
    (work c f dim s).shutdown = s.shutdown := (work_frame s).2.1

theorem workN_shutdown {c : Cfg} {f : Fwd} {dim : Nat} (n : Nat) (s : St) :
    (workN c f dim n s).shutdown = s.shutdown := by
  induction n generalizing s with
  | zero => rfl
  | succ n ih => simp only [workN, ih, work_shutdown]

/-- Nothing queued and nothing in the worker's hands. -/
def Rest (s : St) : Prop := s.queue = [] ∧ s.pc.hand = []

/-- While the embedder lives, every request is answered once the worker has
caught up: left alone, a running worker reaches an idle wait on an empty
queue (or its exit) within `measure s` steps, and then every request ever
queued has been observed exactly once. -/
theorem served_when_left_alone {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch) {s : St}
    (hg : Good c f dim s) (hw : 1 ≤ c.width) (hd : s.pc ≠ .down) :
    Rest (workN c f dim (measure s) s) ∧
      ∀ r ∈ s.issued, ((workN c f dim (measure s) s).obs.map Prod.fst).count r = 1 := by
  have hnb := workN_idle c f dim hm (measure s) s (Nat.le_refl _)
  have hnd := workN_pc_ne_down (c := c) (f := f) (dim := dim) (measure s) hd
  have hg' : Good c f dim (workN c f dim (measure s) s) := by
    rw [workN_eq_run]; exact good_run hw s hg _
  have hiss : (workN c f dim (measure s) s).issued = s.issued := by
    have : ∀ n s, (workN c f dim n s).issued = s.issued := by
      intro n; induction n with
      | zero => intro s; rfl
      | succ n ih => intro s; simp only [workN, ih, (work_frame s).1]
    exact this _ _
  have hrest : Rest (workN c f dim (measure s) s) := by
    unfold Busy at hnb
    cases hpc : (workN c f dim (measure s) s).pc with
    | down => exact absurd hpc hnd
    | exited => exact ⟨(hg'.exitedQ hpc).1, by simp [Pc.hand, hpc]⟩
    | idle =>
      rw [hpc] at hnb
      simp only [not_or, Decidable.not_not] at hnb
      exact ⟨hnb.1, by simp [Pc.hand, hpc]⟩
    | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
    | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
    | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb
  refine ⟨hrest, ?_⟩
  intro r hr
  rw [← hiss] at hr
  exact core_at_rest hg'.toCore hrest.1 hrest.2 r hr

/-- Mirrors test `shutdown_exits_only_after_draining_queued_work`: after
shutdown is requested, a running worker exits within `measure s` steps, and
only once nothing is queued or held: every request has been answered. -/
theorem shutdown_drains {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch) {s : St}
    (hg : Good c f dim s) (hw : 1 ≤ c.width) (hd : s.pc ≠ .down) (hsd : s.shutdown = true) :
    (workN c f dim (measure s) s).pc = .exited ∧ Rest (workN c f dim (measure s) s) ∧
      ∀ r ∈ s.issued, ((workN c f dim (measure s) s).obs.map Prod.fst).count r = 1 := by
  obtain ⟨hrest, hall⟩ := served_when_left_alone hm hg hw hd
  refine ⟨?_, hrest, hall⟩
  have hnb := workN_idle c f dim hm (measure s) s (Nat.le_refl _)
  have hnd := workN_pc_ne_down (c := c) (f := f) (dim := dim) (measure s) hd
  have hs' := workN_shutdown (c := c) (f := f) (dim := dim) (measure s) s
  unfold Busy at hnb
  cases hpc : (workN c f dim (measure s) s).pc with
  | down => exact absurd hpc hnd
  | exited => rfl
  | idle => rw [hpc] at hnb; simp only [not_or] at hnb; exact absurd (hs'.trans hsd) hnb.2
  | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
  | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
  | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb


/-- An accounting theorem, not liveness: whatever state the embedder is in,
request shutdown (the last handle drops), let the worker run, let the
supervisor exit and `Shared` drop, and every request ever queued has been
observed exactly once, so no request is ever left without an outcome. In a
wedged or dead state (worker `down`, supervisor gone) those outcomes are
`.dropped` from `dropShared`, which callers see as a permanent `Backend`
error, not an answer. And the recipe requests shutdown while callers may
still be waiting, which the Rust cannot reach: an awaiting `embed()` keeps a
handle alive. The liveness theorem is `served_when_left_alone`, for a
running worker (`pc ≠ down`). -/
theorem eventually_all_answered {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch)
    (hw : 1 ≤ c.width) {s : St} (hg : Good c f dim s) :
    let evs := [Ev.shutdown] ++ List.replicate (measure (step c f dim s .shutdown)) .work ++
      [.supervise true, .dropShared]
    Rest (run c f dim s evs) ∧ ∀ r ∈ s.issued, ((run c f dim s evs).obs.map Prod.fst).count r = 1 := by
  intro evs
  have hg1 := good_step hw hg .shutdown
  have hiss1 : (step c f dim s .shutdown).issued = s.issued := rfl
  have hsd1 : (step c f dim s .shutdown).shutdown = true := rfl
  generalize hs1 : step c f dim s .shutdown = s1 at hg1 hiss1 hsd1
  have hrun : run c f dim s evs =
      run c f dim (workN c f dim (measure s1) s1) [.supervise true, .dropShared] := by
    simp only [evs, run, List.foldl_append, List.foldl_cons, List.foldl_nil, hs1]
    rw [← run, ← workN_eq_run]
  rw [hrun]
  have hiss2 : (workN c f dim (measure s1) s1).issued = s1.issued := by
    have : ∀ n s, (workN c f dim n s).issued = s.issued := by
      intro n; induction n with
      | zero => intro s; rfl
      | succ n ih => intro s; simp only [workN, ih, (work_frame s).1]
    exact this _ _
  have hg2 : Good c f dim (workN c f dim (measure s1) s1) := by
    rw [workN_eq_run]; exact good_run hw _ hg1 _
  have hsd2 : (workN c f dim (measure s1) s1).shutdown = true := by
    rw [workN_shutdown]; exact hsd1
  have hnb := workN_idle c f dim hm (measure s1) s1 (Nat.le_refl _)
  generalize workN c f dim (measure s1) s1 = s2 at hiss2 hg2 hsd2 hnb
  have hpc2 : s2.pc = .down ∨ s2.pc = .exited := by
    unfold Busy at hnb
    cases hpc : s2.pc with
    | down => exact Or.inl rfl
    | exited => exact Or.inr rfl
    | idle => rw [hpc] at hnb; simp only [not_or] at hnb; exact absurd hsd2 hnb.2
    | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
    | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
    | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb
  have hh2 : s2.pc.hand = [] := by rcases hpc2 with h | h <;> rw [h] <;> rfl
  -- the supervisor exits
  have hr2 : run c f dim s2 [.supervise true, .dropShared] =
      step c f dim (step c f dim s2 (.supervise true)) .dropShared := rfl
  rw [hr2]
  have hg3 := good_step hw hg2 (.supervise true)
  have h3 : (step c f dim s2 (.supervise true)).pc = s2.pc ∧
      (step c f dim s2 (.supervise true)).sup = false ∧
      (step c f dim s2 (.supervise true)).queue = s2.queue ∧
      (step c f dim s2 (.supervise true)).live = s2.live ∧
      (step c f dim s2 (.supervise true)).shutdown = true ∧
      (step c f dim s2 (.supervise true)).issued = s2.issued := by
    simp only [step]
    split
    · simp [hsd2]
    · next h =>
      have : s2.sup = false := by
        cases hs : s2.sup
        · rfl
        · exact absurd ⟨hs, hpc2⟩ h
      exact ⟨rfl, this, rfl, rfl, hsd2, rfl⟩
  generalize step c f dim s2 (.supervise true) = s3 at hg3 h3
  obtain ⟨hpc3, hsup3, hq3, hl3, hsd3, hiss3⟩ := h3
  have hg4 := good_step hw hg3 .dropShared
  have hrest : Rest (step c f dim s3 .dropShared) := by
    simp only [step]
    split
    · rw [settleAll_eq]; refine ⟨rfl, ?_⟩; simp [hpc3, hh2]
    · next h =>
      have hlive : s3.live = false := by
        cases hl : s3.live
        · rfl
        · exact absurd ⟨hl, hsd3, hsup3, hpc3 ▸ hpc2⟩ h
      exact ⟨(hg3.deadQ hlive).1, by rw [hpc3]; exact hh2⟩
  have hiss4 : (step c f dim s3 .dropShared).issued = s3.issued := by
    simp only [step]; split
    · rw [settleAll_eq]
    · rfl
  refine ⟨hrest, ?_⟩
  intro r hr
  have : r ∈ (step c f dim s3 .dropShared).issued := by
    rw [hiss4, hiss3, hiss2, hiss1]; exact hr
  exact core_at_rest hg4.toCore hrest.1 hrest.2 r this


/-! ## No cross-wiring, and isolation over whole runs -/

/-- Where a new observation comes from: a caller's own timeout, a dropped
sender, or an answer being sent, which is the `out` entry of its own
request. -/
theorem step_obs_new {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (e : Ev) :
    ∀ p ∈ (step c f dim s e).obs, p ∈ s.obs ∨ p.2 = .dropped ∨ p.2 = .timedOut ∨
      ∃ t out, s.pc = .sending t out ∧ ∃ o ∈ out, p = (o.1, .res o.2) := by
  intro p hp
  cases e with
  | enqueue r => simp only [step] at hp; split at hp <;> exact Or.inl hp
  | enqueueMany rs => simp only [step] at hp; split at hp <;> exact Or.inl hp
  | giveUp r =>
    simp only [step] at hp; split at hp
    · rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp at h; subst h; exact Or.inr (Or.inr (Or.inl rfl))
    · exact Or.inl hp
  | shutdown => exact Or.inl hp
  | die =>
    simp only [step] at hp; split at hp
    · rw [settleAll_eq] at hp
      rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    · exact Or.inl hp
  | supervise ok =>
    simp only [step] at hp
    split at hp
    · split at hp
      · exact Or.inl hp
      · split at hp
        · split at hp <;> exact Or.inl hp
        · exact Or.inl hp
    · exact Or.inl hp
  | dropShared =>
    simp only [step] at hp; split at hp
    · rw [settleAll_eq] at hp
      rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    · exact Or.inl hp
  | work =>
    simp only [step] at hp
    unfold work at hp
    cases hpc : s.pc with
    | down => simp only [hpc] at hp; exact Or.inl hp
    | exited => simp only [hpc] at hp; exact Or.inl hp
    | idle =>
      simp only [hpc] at hp
      split at hp
      · split at hp <;> exact Or.inl hp
      · split at hp
        · next hq _ =>
          have hq' : s.queue = [] := by simpa using hq
          rw [hq', settleAll_eq] at hp; simp at hp; exact Or.inl hp
        · exact Or.inl hp
    | debounce b => simp only [hpc] at hp; exact Or.inl hp
    | serving t =>
      simp only [hpc] at hp
      split at hp
      · exact Or.inl hp
      · split at hp <;>
        · rw [settleAll_eq] at hp
          rcases List.mem_append.mp hp with h | h
          · exact Or.inl h
          · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    | sending t out =>
      simp only [hpc] at hp
      split at hp
      · exact Or.inl hp
      · next o =>
        rw [settle_eq, settleAll_eq] at hp
        rcases List.mem_append.mp hp with h | h
        · exact Or.inl h
        · simp only [List.mem_map, List.mem_filter, List.mem_singleton] at h
          obtain ⟨_, ⟨rfl, _⟩, rfl⟩ := h
          exact Or.inr (Or.inr (Or.inr ⟨t, [o], rfl, o, by simp, rfl⟩))
      · next o os _ =>
        rw [settle_eq, settleAll_eq] at hp
        rcases List.mem_append.mp hp with h | h
        · exact Or.inl h
        · simp only [List.mem_map, List.mem_filter, List.mem_singleton] at h
          obtain ⟨_, ⟨rfl, _⟩, rfl⟩ := h
          exact Or.inr (Or.inr (Or.inr ⟨t, o :: os, rfl, o, by simp, rfl⟩))

/-- Every vector or error a caller is answered with is its own text's. -/
def IsoObs (row : Txt → Option Vec) (dim : Nat) (s : St) : Prop :=
  ∀ r x, (r, Obs.res x) ∈ s.obs → x = soloRes row dim r.txt

/-- With a row-wise forward of fixed width and a canary that embeds, an
answer being sent is its own request's solo answer. -/
theorem sending_own (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} {s : St} (hg : Good c (rowFwd row) dim s)
    {t : Take} {out : List (Req × Res)} (hpc : s.pc = .sending t out) :
    ∀ o ∈ out, o.2 = soloRes row dim o.1.txt := by
  obtain ⟨pre, k, hwd⟩ := hg.wired t out hpc
  intro o ho
  by_cases hne : (pre ++ out).map (·.1.txt) = []
  · simp at hne; rw [hne.2] at ho; simp at ho
  · rw [forwardIsolated_rowwise row dim w k hu hc _ hne, List.map_map] at hwd
    exact map_eq_pointwise hwd o (List.mem_append_right _ ho)

theorem iso_step (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} {s : St} (hg : Good c (rowFwd row) dim s)
    (hi : IsoObs row dim s) (e : Ev) : IsoObs row dim (step c (rowFwd row) dim s e) := by
  intro r x hx
  rcases step_obs_new e _ hx with h | h | h | ⟨t, out, hpc, o, ho, hp⟩
  · exact hi r x h
  · cases h
  · cases h
  · simp only [Prod.mk.injEq, Obs.res.injEq] at hp
    obtain ⟨rfl, rfl⟩ := hp
    exact sending_own row dim w hu hc hg hpc o ho

/-- #125 over whole runs, through reordering by length, the per-text
reruns, interleaved arrivals, timeouts, deaths and restarts: with a row-wise
forward of fixed width and a canary that embeds, every answer a caller
receives is exactly what its own text gets alone. No caller ever receives
another caller's vector, and no text's failure reaches another caller. -/
theorem answers_are_own (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} (hw : 1 ≤ c.width) (evs : List Ev) :
    IsoObs row dim (run c (rowFwd row) dim St.init evs) := by
  suffices ∀ s, Good c (rowFwd row) dim s → IsoObs row dim s →
      IsoObs row dim (run c (rowFwd row) dim s evs) from
    this _ (good_init c _ dim) (by intro r x hx; simp [St.init] at hx)
  induction evs with
  | nil => intro s _ hi; exact hi
  | cons e es ih =>
    intro s hg hi
    exact ih _ (good_step hw hg e) (iso_step row dim w hu hc hg hi e)

/-- No caller is answered twice, in any run. -/
theorem answered_at_most_once {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev)
    (r : Req) : ((run c f dim St.init evs).obs.map Prod.fst).count r ≤ 1 :=
  (good_reachable f dim hw evs).obsUniq r

/-- Every forward the device runs, in any run, holds at most `FORWARD_WIDTH`
texts, each within the length slack of every other. -/
theorem forwards_capped {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev) :
    ∀ b ∈ (run c f dim St.init evs).fwds, b.length ≤ c.width ∧ Slack c b :=
  (good_reachable f dim hw evs).fwdCaps

/-- A take, topped up, holds at most `MAX_BATCH` requests. -/
theorem topUp_cap (b q : List Req) (n : Nat) (h : b.length ≤ n) :
    (b ++ (takePrioritized q (n - b.length)).1).length ≤ n := by
  have := takePrioritized_length_le q (n - b.length)
  simp only [List.length_append]; omega

/-- The serving step's pulls obey `drive`'s discipline: at most `MAX_BATCH`
singles, and none once the take has no bulk texts, so `take_ends` bounds
every take of the state machine. -/
theorem pullOf_drive (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    (pullOf c sd q t).1.length ≤ c.maxBatch ∧ (t.bulk = [] → (pullOf c sd q t).1 = []) :=
  ⟨pullOf_length c sd q t, fun h => by rw [pullOf_nil c sd q t (Or.inl h)]⟩

/-- No cross-wiring for any forward: in every run, the answers still to be
sent (`out`) are a suffix of a list whose answers are `forward_isolated`'s
results, position by position, on that list's own texts. `pre` is
existential: the invariant does not record that it is the part of the same
forward already sent, nor which requests it held, so the theorem pins each
pending answer to its own request's position in one `forward_isolated`
call, and says nothing about requests already answered (those are covered,
for a row-wise forward, by `answers_are_own`). -/
theorem answers_wired {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev)
    (t : Take) (out : List (Req × Res)) (h : (run c f dim St.init evs).pc = .sending t out) :
    ∃ pre k, (pre ++ out).map Prod.snd = (forwardIsolated f dim k ((pre ++ out).map (·.1.txt))).1 :=
  (good_reachable f dim hw evs).wired t out h

end Lambo.BatchQueue
