/-
# Store-wide session discovery: the `/api/sessions` single-flight read (#276)

Model of `Discovery::sessions` in `src/cli/serve_web/discovery.rs`: the
store-wide read (`GraphStore::store_health`) behind the `/api/sessions`
listing when `[web] discover_sessions` is on, single-flighted so the portal
issues one read at a time, and the rule that decides when a request may take
the outcome of a read it did not start itself.

## Model

* A session is a `Nat`; `St.live` is the store's live, non-erased sessions.
  `Ev.erase s` is `lambo erase-session s` (a `lambo:erased` tombstone): `s`
  leaves the live set at that instant and is logged in `St.erased` with the
  time. Nothing re-creates a session, so an erased session stays out.
* Time is the number of events so far (`St.time`); every event happens at
  `st.time` and advances it by one (`step`), so "before" is strict.
* `Ev.arrive r` is request `r` entering `sessions`: it records the counter
  the rule reads (`snapOf`; the Rust `arrived`) and queues on the gate
  (`gate: AsyncMutex<()>`). `Ev.take r` is request `r` getting the gate, in
  any order (the model does not rely on tokio's FIFO fairness). With the gate
  it either takes the latest finished read's outcome (`reuse`, the early
  `return outcome.clone()`) and lets the gate go, or starts a read and holds
  the gate until it ends. A read observes the live set when it starts
  (`Read.seen`): any instant inside the read is at or after its start, so
  observing at the start is the earliest, most stale, choice.
  `Ev.finish` ends the running read: its owner lists what it saw and the
  outcome becomes `St.last`. `Ev.abandon` is the owner's future being
  dropped mid-read (the client went away): the gate is released and nothing
  is stored.
* `St.started` / `St.completed` count reads started and finished; a read's
  number (`Read.seq`, `Last.seq`) is `started` once it began, and
  `Last.fin` is `completed` once it finished.

Two join rules (`Rule`):

* `Rule.old` is the code as #287 first proposed it: a request records
  `completed` on arrival and takes the outcome of any read that finished
  since (`last.completed > arrived`), including a read that was already
  running, and had already looked at the store, when the request arrived.
* `Rule.fixed` is the fix: a request records `started` on arrival and takes
  only the outcome of a read numbered above it (`seq > arrived`), that is, a
  read that began after it arrived; otherwise it starts a read of its own.
  In the Rust, `Last.started` is bumped under the `last` lock while the gate
  is held, before `store_health` is called, and `Last.finished` keeps the
  latest finished read's number beside its outcome; a dropped owner leaves
  `finished` as it was (`Ev.abandon`). The gate still serialises reads.
  A request that arrives during a read never shares it and costs at most
  one more read of its own; requests that arrive during one read share the
  next read when it finishes (`fixed_shares`), whatever order they take the
  gate in, unless that read is abandoned.

## Results

* `old_lists_erased`, `old_not_fresh`, `old_violates`: the CodeRabbit
  scenario on #287. Request A starts a read that observes sessions 1 and 2;
  session 2 is erased; request B arrives; A's read finishes; B takes A's
  outcome and lists session 2, erased before B arrived.
* `fixed_coderabbit`: the same events under the fix. B reads again and lists
  session 1 only.
* `fixed_fresh`: under the fix, every listing comes from a read that started
  after its request arrived (`Fresh`), on every run.
* `listing_safe` (either rule): no listing names a session erased before the
  read it comes from started. So the old rule's only fault is reusing a read
  older than the request, and with `fixed_fresh`:
* `fixed_no_erased_listed`: under the fix, no request lists a session erased
  before it arrived (`NoErasedListed`): the promise of the module docs.
* `one_read_at_a_time` (either rule): a read starts only when none is
  running, and the running read is one `Option`, so the portal issues at
  most one read at a time. (A read the portal gave up on, timed out or
  dropped, is `Ev.abandon` here; its query may still be running in the
  backend, which the model does not see.)
* `fixed_shares`: the fix still shares. Three requests, the second and third
  arriving during the first one's read, cost two reads; the third takes the
  second's outcome, which began after it arrived.

Boundary: `store_health`'s own query is not modelled (a read observes the live
set at its start, the most stale choice); a read the portal abandoned may
still run in the backend, which the model does not see; tokio's FIFO fairness
is not relied on; the adapters' `store_health` is taken to omit erased
sessions, and the route that appends the outcome to the served sessions
(`api_sessions`) is left out.
-/

namespace Lambo.Discovery

/-- Which join rule `Discovery::sessions` applies. -/
inductive Rule where
  /-- Join any read that finished after the request arrived. -/
  | old
  /-- Join only a read that started after the request arrived. -/
  | fixed
  deriving DecidableEq, Repr

/-- A request waiting on the gate: its id, arrival time, and the counter it
recorded on arrival (the Rust `arrived`). -/
structure Req where
  id : Nat
  arrival : Nat
  snap : Nat
  deriving DecidableEq, Repr

/-- The running store read: whose it is, its number, when it began, and the
live sessions it observed. -/
structure Read where
  owner : Req
  seq : Nat
  start : Nat
  seen : List Nat
  deriving DecidableEq, Repr

/-- The latest finished read (`Last` in the Rust): its number, the count of
reads finished with it, when it began, and its outcome. -/
structure Last where
  seq : Nat
  fin : Nat
  start : Nat
  listing : List Nat
  deriving DecidableEq, Repr

/-- A listing a request returned, and when the read it comes from began. -/
structure Answer where
  id : Nat
  arrival : Nat
  readStart : Nat
  listing : List Nat
  deriving DecidableEq, Repr

/-- The portal's discovery state, the store's live set, and ghost history. -/
structure St where
  time : Nat
  live : List Nat
  erased : List (Nat × Nat)
  started : Nat
  completed : Nat
  running : Option Read
  last : Option Last
  queue : List Req
  answers : List Answer
  deriving DecidableEq, Repr

/-- `Discovery::new`: no read yet; the store holds `live`. -/
def St.init (live : List Nat) : St :=
  { time := 0, live, erased := [], started := 0, completed := 0, running := none,
    last := none, queue := [], answers := [] }

/-- What happens next. -/
inductive Ev where
  | arrive (r : Nat)
  | erase (s : Nat)
  | take (r : Nat)
  | finish
  | abandon
  deriving DecidableEq, Repr

/-- The counter a request records on arrival. -/
def snapOf : Rule → St → Nat
  | .old, st => st.completed
  | .fixed, st => st.started

/-- May a request holding the gate take `l`'s outcome? -/
def joins : Rule → Req → Last → Bool
  | .old, q, l => decide (q.snap < l.fin)
  | .fixed, q, l => decide (q.snap < l.seq)

/-- The outcome a request holding the gate takes, if any. -/
def reuse (rule : Rule) (q : Req) : Option Last → Option Last
  | some l => if joins rule q l then some l else none
  | none => none

def arrive (rule : Rule) (r : Nat) (st : St) : St :=
  { st with queue := st.queue ++ [⟨r, st.time, snapOf rule st⟩] }

def erase (s : Nat) (st : St) : St :=
  { st with live := st.live.filter (fun x => x != s), erased := (s, st.time) :: st.erased }

/-- Request `r` gets the gate, if it is free and `r` is waiting. -/
def take (rule : Rule) (r : Nat) (st : St) : St :=
  match st.running with
  | some _ => st
  | none =>
    match st.queue.find? (fun q => q.id == r) with
    | none => st
    | some q =>
      match reuse rule q st.last with
      | some l =>
        { st with queue := st.queue.erase q,
                  answers := ⟨q.id, q.arrival, l.start, l.listing⟩ :: st.answers }
      | none =>
        { st with queue := st.queue.erase q, started := st.started + 1,
                  running := some ⟨q, st.started + 1, st.time, st.live⟩ }

/-- The running read returns: its owner lists what it saw, and it becomes the
latest finished read. -/
def finish (st : St) : St :=
  match st.running with
  | none => st
  | some r =>
    { st with running := none, completed := st.completed + 1,
              last := some ⟨r.seq, st.completed + 1, r.start, r.seen⟩,
              answers := ⟨r.owner.id, r.owner.arrival, r.start, r.seen⟩ :: st.answers }

/-- The running read's owner is dropped: the gate is free, nothing is stored. -/
def abandon (st : St) : St := { st with running := none }

def apply (rule : Rule) (st : St) : Ev → St
  | .arrive r => arrive rule r st
  | .erase s => erase s st
  | .take r => take rule r st
  | .finish => finish st
  | .abandon => abandon st

def step (rule : Rule) (st : St) (e : Ev) : St :=
  { apply rule st e with time := st.time + 1 }

def run (rule : Rule) (st : St) (evs : List Ev) : St := evs.foldl (step rule) st

/-- Every listing comes from a read that began after its request arrived. -/
def Fresh (st : St) : Prop := ∀ a ∈ st.answers, a.arrival < a.readStart

/-- No listing names a session erased before its request arrived. -/
def NoErasedListed (st : St) : Prop :=
  ∀ a ∈ st.answers, ∀ e ∈ st.erased, e.2 < a.arrival → e.1 ∉ a.listing

theorem run_induct (P : St → Prop) (rule : Rule)
    (hstep : ∀ st e, P st → P (step rule st e)) :
    ∀ (evs : List Ev) (st : St), P st → P (run rule st evs)
  | [], _, h => h
  | e :: evs, st, h => run_induct P rule hstep evs (step rule st e) (hstep st e h)

/-! ## The previous rule lists an erased session -/

/-- The CodeRabbit scenario: A arrives and starts a read that observes 1 and
2; 2 is erased; B arrives; A's read finishes; B gets the gate; (a read, if
B started one, finishes). -/
def coderabbit : List Ev :=
  [.arrive 0, .take 0, .erase 2, .arrive 1, .finish, .take 1, .finish]

theorem old_lists_erased :
    (run .old (St.init [1, 2]) coderabbit).answers =
      [⟨1, 3, 1, [1, 2]⟩, ⟨0, 0, 1, [1, 2]⟩] ∧
    (run .old (St.init [1, 2]) coderabbit).erased = [(2, 2)] := by
  decide

/-- Under #287's first rule, `Discovery::sessions` can answer a request with a
listing read before the request arrived. -/
theorem old_not_fresh : ¬ Fresh (run .old (St.init [1, 2]) coderabbit) := by
  intro h
  have := h ⟨1, 3, 1, [1, 2]⟩ (by rw [old_lists_erased.1]; exact List.mem_cons_self ..)
  exact absurd this (by decide)

/-- Under #287's first rule (join any read that finished since arrival),
`Discovery::sessions` can list a session erased before the request's own read
began. -/
theorem old_violates : ¬ NoErasedListed (run .old (St.init [1, 2]) coderabbit) := by
  intro h
  have := h ⟨1, 3, 1, [1, 2]⟩ (by rw [old_lists_erased.1]; exact List.mem_cons_self ..)
    (2, 2) (by rw [old_lists_erased.2]; exact List.mem_cons_self ..) (by decide)
  exact this (by decide)

/-! ## The fix -/

/-- The fixed `Discovery::sessions` on the review's scenario: each request is
answered by a read that started after it arrived. -/
theorem fixed_coderabbit :
    (run .fixed (St.init [1, 2]) coderabbit).answers =
      [⟨1, 3, 5, [1]⟩, ⟨0, 0, 1, [1, 2]⟩] := by
  decide

/-- Two requests arriving during a read share the next one: three requests,
two reads. -/
def sharing : List Ev :=
  [.arrive 0, .take 0, .arrive 1, .arrive 2, .finish, .take 1, .take 2, .finish, .take 2]

/-- The fixed `Discovery::sessions` still shares reads: three requests are
served by two reads. -/
theorem fixed_shares :
    (run .fixed (St.init [1]) sharing).started = 2 ∧
    (run .fixed (St.init [1]) sharing).answers =
      [⟨2, 3, 5, [1]⟩, ⟨1, 2, 5, [1]⟩, ⟨0, 0, 1, [1]⟩] := by
  decide

/-- What the fix keeps true at every step. -/
structure FreshInv (st : St) : Prop where
  answers : ∀ a ∈ st.answers, a.arrival < a.readStart
  queue : ∀ q ∈ st.queue, q.arrival < st.time ∧ q.snap ≤ st.started
  queueLast : ∀ q ∈ st.queue, ∀ l, st.last = some l → q.snap < l.seq → q.arrival < l.start
  queueRun : ∀ q ∈ st.queue, ∀ r, st.running = some r → q.snap < r.seq → q.arrival < r.start
  running : ∀ r, st.running = some r → r.owner.arrival < r.start ∧ r.seq ≤ st.started
  last : ∀ l, st.last = some l → l.seq ≤ st.started

theorem freshInv_init (live : List Nat) : FreshInv (St.init live) where
  answers := by intro a h; cases h
  queue := by intro q h; cases h
  queueLast := by intro q h; cases h
  queueRun := by intro q h; cases h
  running := by intro r h; cases h
  last := by intro l h; cases h

theorem freshInv_step (st : St) (e : Ev) (h : FreshInv st) :
    FreshInv (step .fixed st e) := by
  cases e with
  | arrive r =>
    refine ⟨h.answers, ?_, ?_, ?_, h.running, h.last⟩
    · intro q hq
      rcases List.mem_append.mp hq with hq | hq
      · have := h.queue q hq
        exact ⟨Nat.lt_succ_of_lt this.1, this.2⟩
      · rw [List.mem_singleton.mp hq]
        exact ⟨Nat.lt_succ_self _, Nat.le_refl _⟩
    · intro q hq l hl hs
      rcases List.mem_append.mp hq with hq | hq
      · exact h.queueLast q hq l hl hs
      · rw [List.mem_singleton.mp hq] at hs
        have := h.last l hl
        exact absurd hs (Nat.not_lt.mpr this)
    · intro q hq r hr hs
      rcases List.mem_append.mp hq with hq | hq
      · exact h.queueRun q hq r hr hs
      · rw [List.mem_singleton.mp hq] at hs
        have := (h.running r hr).2
        exact absurd hs (Nat.not_lt.mpr this)
  | erase s =>
    refine ⟨h.answers, ?_, h.queueLast, h.queueRun, h.running, h.last⟩
    intro q hq
    have := h.queue q hq
    exact ⟨Nat.lt_succ_of_lt this.1, this.2⟩
  | take r =>
    show FreshInv { take .fixed r st with time := st.time + 1 }
    unfold take
    split
    · exact ⟨h.answers, fun q hq => ⟨Nat.lt_succ_of_lt (h.queue q hq).1, (h.queue q hq).2⟩,
        h.queueLast, h.queueRun, h.running, h.last⟩
    · rename_i hrun
      split
      · exact ⟨h.answers, fun q hq => ⟨Nat.lt_succ_of_lt (h.queue q hq).1, (h.queue q hq).2⟩,
          h.queueLast, h.queueRun, h.running, h.last⟩
      · rename_i q hfind
        have hqmem : q ∈ st.queue := List.mem_of_find?_eq_some hfind
        split
        · rename_i l hreuse
          have hl : st.last = some l ∧ q.snap < l.seq := by
            unfold reuse at hreuse
            split at hreuse
            · rename_i l' hlast
              split at hreuse
              · rename_i hj
                cases hreuse
                exact ⟨hlast, by simpa [joins] using hj⟩
              · cases hreuse
            · cases hreuse
          refine ⟨?_, ?_, ?_, ?_, h.running, h.last⟩
          · intro a ha
            rcases List.mem_cons.mp ha with ha | ha
            · rw [ha]; exact h.queueLast q hqmem l hl.1 hl.2
            · exact h.answers a ha
          · intro q' hq'
            have := h.queue q' (List.mem_of_mem_erase hq')
            exact ⟨Nat.lt_succ_of_lt this.1, this.2⟩
          · intro q' hq'
            exact h.queueLast q' (List.mem_of_mem_erase hq')
          · intro q' hq'
            exact h.queueRun q' (List.mem_of_mem_erase hq')
        · refine ⟨h.answers, ?_, ?_, ?_, ?_, ?_⟩
          · intro q' hq'
            have := h.queue q' (List.mem_of_mem_erase hq')
            exact ⟨Nat.lt_succ_of_lt this.1, Nat.le_succ_of_le this.2⟩
          · intro q' hq'
            exact h.queueLast q' (List.mem_of_mem_erase hq')
          · intro q' hq' r' hr' _
            cases hr'
            exact (h.queue q' (List.mem_of_mem_erase hq')).1
          · intro r' hr'
            cases hr'
            exact ⟨(h.queue q hqmem).1, Nat.le_refl _⟩
          · intro l hl
            exact Nat.le_succ_of_le (h.last l hl)
  | finish =>
    show FreshInv { finish st with time := st.time + 1 }
    unfold finish
    split
    · exact ⟨h.answers, fun q hq => ⟨Nat.lt_succ_of_lt (h.queue q hq).1, (h.queue q hq).2⟩,
        h.queueLast, h.queueRun, h.running, h.last⟩
    · rename_i r hr
      refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
      · intro a ha
        rcases List.mem_cons.mp ha with ha | ha
        · rw [ha]; exact (h.running r hr).1
        · exact h.answers a ha
      · intro q hq
        have := h.queue q hq
        exact ⟨Nat.lt_succ_of_lt this.1, this.2⟩
      · intro q hq l hl hs
        cases hl
        exact h.queueRun q hq r hr hs
      · intro q _ r' hr'
        cases hr'
      · intro r' hr'
        cases hr'
      · intro l hl
        cases hl
        exact (h.running r hr).2
  | abandon =>
    refine ⟨h.answers, ?_, h.queueLast, ?_, ?_, h.last⟩
    · intro q hq
      have := h.queue q hq
      exact ⟨Nat.lt_succ_of_lt this.1, this.2⟩
    · intro q _ r' hr'
      cases hr'
    · intro r' hr'
      cases hr'

/-- **Freshness.** Under the fix, on every run, every listing comes from a
store read that began after its request arrived. -/
theorem fixed_fresh (live : List Nat) (evs : List Ev) :
    Fresh (run .fixed (St.init live) evs) :=
  (run_induct FreshInv .fixed freshInv_step evs _ (freshInv_init live)).answers

/-! ## What a read can list (either rule) -/

/-- A read lists nothing erased before it began. -/
structure SafeInv (st : St) : Prop where
  gone : ∀ e ∈ st.erased, e.1 ∉ st.live ∧ e.2 < st.time
  answers : ∀ a ∈ st.answers,
    a.readStart < st.time ∧ ∀ e ∈ st.erased, e.2 < a.readStart → e.1 ∉ a.listing
  running : ∀ r, st.running = some r →
    r.start < st.time ∧ ∀ e ∈ st.erased, e.2 < r.start → e.1 ∉ r.seen
  last : ∀ l, st.last = some l →
    l.start < st.time ∧ ∀ e ∈ st.erased, e.2 < l.start → e.1 ∉ l.listing

theorem safeInv_init (live : List Nat) : SafeInv (St.init live) where
  gone := by intro e h; cases h
  answers := by intro a h; cases h
  running := by intro r h; cases h
  last := by intro l h; cases h

/-- Advancing the clock keeps `SafeInv` when nothing else changed. -/
theorem safeInv_tick (st : St) (h : SafeInv st) : SafeInv { st with time := st.time + 1 } where
  gone := fun e he => ⟨(h.gone e he).1, Nat.lt_succ_of_lt (h.gone e he).2⟩
  answers := fun a ha => ⟨Nat.lt_succ_of_lt (h.answers a ha).1, (h.answers a ha).2⟩
  running := fun r hr => ⟨Nat.lt_succ_of_lt (h.running r hr).1, (h.running r hr).2⟩
  last := fun l hl => ⟨Nat.lt_succ_of_lt (h.last l hl).1, (h.last l hl).2⟩

theorem safeInv_step (rule : Rule) (st : St) (e : Ev) (h : SafeInv st) :
    SafeInv (step rule st e) := by
  cases e with
  | arrive r =>
    have t := safeInv_tick st h
    exact ⟨t.gone, t.answers, t.running, t.last⟩
  | erase s =>
    refine ⟨?_, ?_, ?_, ?_⟩
    · intro e he
      rcases List.mem_cons.mp he with he | he
      · rw [he]
        refine ⟨?_, Nat.lt_succ_self _⟩
        intro hm
        have := (List.mem_filter.mp hm).2
        simp at this
      · refine ⟨?_, Nat.lt_succ_of_lt (h.gone e he).2⟩
        intro hm
        exact (h.gone e he).1 (List.mem_filter.mp hm).1
    · intro a ha
      refine ⟨Nat.lt_succ_of_lt (h.answers a ha).1, ?_⟩
      intro e he hlt
      rcases List.mem_cons.mp he with he | he
      · rw [he] at hlt
        exact absurd (h.answers a ha).1 (Nat.not_lt.mpr (Nat.le_of_lt hlt))
      · exact (h.answers a ha).2 e he hlt
    · intro r hr
      refine ⟨Nat.lt_succ_of_lt (h.running r hr).1, ?_⟩
      intro e he hlt
      rcases List.mem_cons.mp he with he | he
      · rw [he] at hlt
        exact absurd (h.running r hr).1 (Nat.not_lt.mpr (Nat.le_of_lt hlt))
      · exact (h.running r hr).2 e he hlt
    · intro l hl
      refine ⟨Nat.lt_succ_of_lt (h.last l hl).1, ?_⟩
      intro e he hlt
      rcases List.mem_cons.mp he with he | he
      · rw [he] at hlt
        exact absurd (h.last l hl).1 (Nat.not_lt.mpr (Nat.le_of_lt hlt))
      · exact (h.last l hl).2 e he hlt
  | take r =>
    show SafeInv { take rule r st with time := st.time + 1 }
    unfold take
    split
    · exact safeInv_tick st h
    · split
      · exact safeInv_tick st h
      · rename_i q _
        split
        · rename_i l hreuse
          have hl : st.last = some l := by
            unfold reuse at hreuse
            split at hreuse
            · split at hreuse
              · cases hreuse; assumption
              · cases hreuse
            · cases hreuse
          refine ⟨(safeInv_tick st h).gone, ?_, (safeInv_tick st h).running,
            (safeInv_tick st h).last⟩
          intro a ha
          rcases List.mem_cons.mp ha with ha | ha
          · rw [ha]
            exact ⟨Nat.lt_succ_of_lt (h.last l hl).1, (h.last l hl).2⟩
          · exact (safeInv_tick st h).answers a ha
        · refine ⟨(safeInv_tick st h).gone, (safeInv_tick st h).answers, ?_,
            (safeInv_tick st h).last⟩
          intro r' hr'
          cases hr'
          exact ⟨Nat.lt_succ_self _, fun e he _ => (h.gone e he).1⟩
  | finish =>
    show SafeInv { finish st with time := st.time + 1 }
    unfold finish
    split
    · exact safeInv_tick st h
    · rename_i r hr
      refine ⟨(safeInv_tick st h).gone, ?_, ?_, ?_⟩
      · intro a ha
        rcases List.mem_cons.mp ha with ha | ha
        · rw [ha]
          exact ⟨Nat.lt_succ_of_lt (h.running r hr).1, (h.running r hr).2⟩
        · exact (safeInv_tick st h).answers a ha
      · intro r' hr'
        cases hr'
      · intro l hl
        cases hl
        exact ⟨Nat.lt_succ_of_lt (h.running r hr).1, (h.running r hr).2⟩
  | abandon =>
    refine ⟨(safeInv_tick st h).gone, (safeInv_tick st h).answers, ?_, (safeInv_tick st h).last⟩
    intro r' hr'
    cases hr'

/-- **A listing is no older than its read.** Under either rule, no listing
names a session erased before the read it comes from began. -/
theorem listing_safe (rule : Rule) (live : List Nat) (evs : List Ev) :
    ∀ a ∈ (run rule (St.init live) evs).answers, ∀ e ∈ (run rule (St.init live) evs).erased,
      e.2 < a.readStart → e.1 ∉ a.listing :=
  fun a ha => ((run_induct SafeInv rule (safeInv_step rule) evs _ (safeInv_init live)).answers
    a ha).2

/-- **The module docs' promise.** Under the fix, no request lists a session
erased before it arrived. -/
theorem fixed_no_erased_listed (live : List Nat) (evs : List Ev) :
    NoErasedListed (run .fixed (St.init live) evs) :=
  fun a ha e he hlt =>
    listing_safe .fixed live evs a ha e he (Nat.lt_trans hlt (fixed_fresh live evs a ha))

/-! ## One read at a time -/

/-- **One read at a time** (either rule). A step starts at most one read, and
only when none is running; the running read is then the new one. -/
theorem one_read_at_a_time (rule : Rule) (st : St) (e : Ev) :
    (step rule st e).started = st.started ∨
      (st.running = none ∧ (step rule st e).started = st.started + 1 ∧
        (step rule st e).running.isSome = true) := by
  cases e with
  | arrive r => exact Or.inl rfl
  | erase s => exact Or.inl rfl
  | finish =>
    left
    show (finish st).started = st.started
    unfold finish
    split <;> rfl
  | abandon => exact Or.inl rfl
  | take r =>
    show (take rule r st).started = st.started ∨
      (st.running = none ∧ (take rule r st).started = st.started + 1 ∧
        (take rule r st).running.isSome = true)
    unfold take
    split
    · exact Or.inl rfl
    · rename_i hrun
      split
      · exact Or.inl rfl
      · split
        · exact Or.inl rfl
        · exact Or.inr ⟨hrun, rfl, rfl⟩

end Lambo.Discovery
