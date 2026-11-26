import LamboProofs.Registry.Pure

/-
# Runs of the registry, replayed

Concrete runs of `Step.lean`'s step relation from a witness serve (`cfgEx`:
one pinned session and two on-demand ids, example settings and the Rust's
fixed constants `ELECTION_RETRY`, `PINNED_RETRY`, `PREVIOUS_HANDLE_WAIT`,
`EVICT_MIN_IDLE` and `NEGATIVE_TTL`), each reachable (`reach_of_run`): the
bounds, the stale-write fence, eviction, the erase and the pinned retry, the
backoff, the #312 watcher fix, and a race in
`src/mcp/serve/registry/on_demand.rs`.

Since #312, a queued lease-loss detach identifies the watched handle, so
it leaves a healthy replacement and its call in flight
(`lost_detach_preserves_replacement`). `Flight`'s `Drop` removes the slot and
only then spawns `release_abandoned`, holder-scoped, so if the session is
attached again before that task runs, it releases the new handle's lease and
the next beat fences the healthy session (`dropped_flight_fences_next`); it
needs a panic or abort of an attach task and the cached `Failed` gone before
the release runs. After a #293 re-take the fenced handle is detached without a
release, so an in-serve erase answers 409 `HeldElsewhere`, which erase.rs
documents as a live writer in another process: a doc mismatch
(`zombieRun`). Past `PREVIOUS_HANDLE_WAIT` an on-demand request attaches a
session whose old handle is still alive, so two handles of one session are
alive at once, as documented (`previous_wait_expires`).

Boundary: each run is one interleaving; the general statements are
`Step.lean`'s. The interleavings delay a spawned task until after a new
attach. The model does not estimate how often the scheduler does this.
-/

namespace Lambo.Registry
/-! ## Runs of the Rust, replayed -/

section Runs

theorem reach_of_run {c : Cfg} {s0 : St} (h0 : Reach c s0) {es : List Ev} {p : St → Bool}
    (h : (run c s0 es).any p = true) : ∃ s, Reach c s ∧ p s = true := by
  cases e : run c s0 es with
  | none => rw [e] at h; cases h
  | some s =>
    rw [e] at h
    exact ⟨s, run_reach h0 e, h⟩

/-- A witness serve: one pinned session (`0`) and two on-demand ids (`1`,
`2`). Two kinds of value, in milliseconds:

* example settings, chosen for the witness and not defaults: `max_attached =
  2` (so `places` 1, one on-demand place, and `cap` `unplacedCap 2`, the
  serve's `unplaced_cap(max_attached)`), `attach_concurrency = 2` (`permits`
  2) and `idle_detach_secs = 60` (`idle` 60000; the default is 900);
* the Rust's fixed constants: `ELECTION_RETRY` (`er` 1000), `PINNED_RETRY`
  (`pr`, `pinnedRetry 1000` = 5000), `PREVIOUS_HANDLE_WAIT` (`wait` 30000),
  `EVICT_MIN_IDLE` (`minIdle` 2000) and `NEGATIVE_TTL` (`ttl` 30000).

Pinned by `formal_constants_tests::registry`: the constants by value; the
settings built as a `ServeConfig`, which `ServeConfig::validate` accepts. -/
def cfgEx : Cfg :=
  { ids := [0, 1, 2], pinned := [0], onDemand := true, places := 1, permits := 2,
    cap := unplacedCap 2, er := 1000, pr := pinnedRetry 1000, wait := 30000, idle := 60000,
    minIdle := 2000, watchers := true, ttl := 30000 }

/-- The pinned session's handle, attached at startup, its watcher parked. -/
def hPin : H :=
  { hid := 100, token := 1, fenced := false, woken := false, beating := true, watch := .parked,
    inflight := 0, used := 0 }

/-- The serve just started: the pinned session live at token 1. -/
def s0 : St :=
  { now := 0, slots := fun j => if j = 0 then some (.live hPin) else none,
    rows := fun j => if j = 0 then ⟨.me, 1⟩ else ⟨.none, 0⟩, free := 2, closing := false,
    retry := none, round := [], lost := [], aband := [], strays := [], prevOD := fun _ => none,
    neg := fun _ => none, next := 0, accepted := [], rel := [], builds := [] }

theorem s0_init : Init cfgEx s0 := by
  refine ⟨rfl, rfl, rfl, fun j => ?_⟩
  by_cases hj : j = 0
  · subst hj
    exact Or.inr ⟨by decide, Or.inl ⟨hPin, rfl⟩⟩
  · left
    simp [s0, hj]

/-- Attach on-demand session `1` with `create`, on a free place. -/
def attach1 : List Ev := [.start 1 true, .fPermit 1, .fProbe 1 false none, .fAdmit 1 true]

/-- **Single flight** (test `concurrent_first_requests_attach_once`): a second
request for a session whose attach is in flight starts nothing. -/
example : (run cfgEx s0 [.start 1 true, .start 1 true]).isNone = true := by decide
example : (run cfgEx s0 [.start 1 true, .fPermit 1, .start 1 false]).isNone = true := by decide

/-! ### The bounds, concretely -/

/-- At the eviction, the attach has taken its place before its victim's detach
ended: two slots take a place under one (`taken` 2), but one `Memory` is held
(`mems_le_places`). -/
example : (run cfgEx s0 (attach1 ++ [.tick 2000, .start 2 true, .fPermit 2,
    .fProbe 2 false (some 1)])).any
    (fun s => taken cfgEx s.slots == 2 && memSum cfgEx s.slots == 1) = true := by decide

/-- Two attaches hold both permits; a third waits (`attaches_le_permits`). -/
example : (run cfgEx s0 [.start 1 true, .start 2 true, .fPermit 1, .fPermit 2]).any
    (fun s => holdSum cfgEx s == 2 && s.free == 0) = true := by decide

/-- With an unplaced cap of one, a second unprobed attach is refused
(`unplaced_le_cap`). -/
example : (run { cfgEx with cap := 1 } s0 [.start 1 true, .start 2 true]).isNone = true := by decide

/-- A call in flight keeps the session through an idle sweep
(`sweep_skips_in_flight`); once it ends, the next sweep takes it. -/
example : (run cfgEx s0 (attach1 ++ [.enter 1, .tick 60000, .sweep])).any
    (fun s => isLive (s.slots 1) == 1) = true := by decide
example : (run cfgEx s0 (attach1 ++ [.enter 1, .tick 60000, .sweep, .leave 1, .tick 60000,
    .sweep])).any (fun s => match s.slots 1 with
      | some (.detaching _ none) => true
      | _ => false) = true := by decide

/-- An attach that times out while acquiring frees its slot and its permit
(`timeout_clears`), and releases, holder-scoped, any lease it took. -/
example : (run cfgEx s0 [.start 1 true, .fPermit 1, .fProbe 1 false none, .fTimeout 1]).any
    (fun s => s.slots 1 == none && s.free == 2) = true := by decide

/-! ### `watch_lease` detaches the watched handle -/

/-- The handle of session `1`'s second attach in `abaRun`: unfenced, a call
in flight. -/
def hSecond : H :=
  { hid := 1, token := 3, fenced := false, woken := false, beating := true, watch := .unpolled,
    inflight := 1, used := 60000 }

/-- Session `1` is attached; another writer takes it over and the heartbeat
latches; the watcher fires and spawns its detach, which does not run yet. The
idle sweeper detaches the fenced handle meanwhile; the slot goes, the handle
is dropped, the other writer lets go, and a request attaches the session
again, healthy, and calls into it. Then the spawned detach runs. -/
def abaRun : List Ev :=
  attach1 ++ [.watchPark 1, .rowLapse 1, .otherTake 1, .beat 1, .watchFire 1, .tick 60000, .sweep,
    .detachRun 1, .strayDrop 0, .rowLapse 1] ++ attach1 ++ [.enter 1, .lostRun 1 0]

/-- A delayed lease-loss detach from the first handle leaves the replacement
live, unfenced and with its call in flight. Before #312 the detach matched
only the session id and cut the replacement's call. -/
theorem lost_detach_preserves_replacement :
    ∃ s, Reach cfgEx s ∧ s.slots 1 = some (.live hSecond) := by
  obtain ⟨s, hs, hp⟩ := reach_of_run (Reach.init s0_init) (es := abaRun)
    (p := fun s => decide (s.slots 1 = some (.live hSecond))) (by decide)
  exact ⟨s, hs, of_decide_eq_true hp⟩

/-! ### A dropped flight's release -/

/-- The handle `dropRun` fences. -/
def hFenced : H :=
  { hid := 0, token := 1, fenced := true, woken := true, beating := false, watch := .unpolled,
    inflight := 0, used := 0 }

/-- An attach task dies before its acquire (a panic, an abort): its `Drop`
removes the slot, caches `Failed` and spawns `release_abandoned`. The cached
`Failed` answers the session's requests for `NEGATIVE_TTL` and starts nothing,
so a re-attach first needs the entry gone: here it is evicted from a full
cache (`negEvict`); waiting out the TTL (`tick 30000`) does as well. Then a
request attaches the session again before the release task runs; then it
runs, and then the new handle's heartbeat beats. -/
def dropRun : List Ev := [.start 1 true, .fDrop 1, .negEvict 1] ++ attach1 ++ [.abandRun 1, .beat 1]

/-- While the `Failed` entry is fresh, no attach of the session starts. -/
example : (run cfgEx s0 [.start 1 true, .fDrop 1, .start 1 true]).isNone = true := by decide
example : (run cfgEx s0 [.start 1 true, .fDrop 1, .tick 29999, .start 1 true]).isNone = true := by
  decide
example : (run cfgEx s0 ([.start 1 true, .fDrop 1, .tick 30000] ++ attach1)).isSome = true := by
  decide

/-- **A dropped flight's release takes the next attach's lease, and #293's
fence then fences that healthy session** (`stale_release_fences`): no other
writer is involved. The writes the session accepted after the release and
before the beat are dropped with its tail. It needs the dropped flight's
`Failed` entry gone before the release task runs: evicted from a full
negative cache (`NEGATIVE_CACHE_MAX` newer entries), or the release task
delayed past `NEGATIVE_TTL` (30 s; the release itself is bounded by
`LEASE_RELEASE_GRACE`, 2 s, so only a store that applies a timed-out release
late gets there).
Pinned by `formal_constants_tests::registry`. -/
theorem dropped_flight_fences_next :
    ∃ s, Reach cfgEx s ∧ s.slots 1 = some (.live hFenced) ∧ s.rows 1 = ⟨.me, 2⟩ := by
  obtain ⟨s, hs, hp⟩ := reach_of_run (Reach.init s0_init) (es := dropRun)
    (p := fun s => decide (s.slots 1 = some (.live hFenced) ∧ s.rows 1 = ⟨.me, 2⟩)) (by decide)
  exact ⟨s, hs, of_decide_eq_true hp⟩

/-! ### The stale-write fence waits for a beat -/

/-- Session `1` attached, its watcher parked, a call in flight; another writer
takes the lease; the flush loop's next flush is refused as stale. -/
def staleRun : List Ev := attach1 ++ [.watchPark 1, .enter 1, .rowLapse 1, .otherTake 1, .stale 1]

/-- After the stale latch alone the watcher does not fire and every write is
refused, while the session stays live and routed... -/
example : (run cfgEx s0 (staleRun ++ [.watchFire 1])).isNone = true := by decide
example : (run cfgEx s0 (staleRun ++ [.write 1])).isNone = true := by decide
example : (run cfgEx s0 staleRun).any (fun s => answer false s.now (s.slots 1) == .live) = true := by
  decide

/-- ...and the next beat wakes it: the detach is spawned. -/
example : (run cfgEx s0 (staleRun ++ [.beat 1, .watchFire 1])).any (fun s => s.lost == [(1, 0)]) = true := by
  decide

/-! ### Eviction -/

/-- Session `1` holds the one on-demand place and has been idle 2 s; a
request for `2` evicts it, waits for its detach, and attaches `2`. Then `1`
again evicts `2` and comes back at the next token. -/
def evictRun : List Ev :=
  attach1 ++ [.tick 2000, .start 2 true, .fPermit 2, .fProbe 2 false (some 1), .detachRun 1, .fGo 2,
    .fAdmit 2 true, .strayDrop 0, .tick 2000, .start 1 true, .fPermit 1, .fProbe 1 false (some 2),
    .detachRun 2, .fGo 1, .fAdmit 1 true]

/-- The test `eviction_releases_the_lease_and_a_reattach_mints_the_next_token`:
the evicted lease is released with its token kept, and the re-attach takes
the next token. -/
example : (run cfgEx s0 (attach1 ++ [.tick 2000, .start 2 true, .fPermit 2,
    .fProbe 2 false (some 1), .detachRun 1])).any
      (fun s => s.rows 1 == ⟨.released, 1⟩ && s.slots 1 == none) = true := by decide
example : (run cfgEx s0 evictRun).any
    (fun s => s.rows 1 == ⟨.me, 2⟩ && s.rows 2 == ⟨.released, 1⟩ && s.slots 2 == none) = true := by
  decide

/-- Without the 2 s idle floor, nothing is evicted (test
`a_session_used_just_now_is_not_evicted`). -/
example : (run cfgEx s0 (attach1 ++ [.start 2 true, .fPermit 2, .fProbe 2 false (some 1)])).isNone =
    true := by decide

/-! ### Erase and the pinned retry -/

/-- The pinned session held elsewhere, its row free again. -/
def sHeld : St :=
  { s0 with slots := fun j => if j = 0 then some (.held 0 none) else none,
            rows := fun j => if j = 0 then ⟨.released, 1⟩ else ⟨.none, 0⟩ }

/-- The test `an_erase_racing_the_pinned_retry_waits_for_it_then_erases_what_it_attached`:
while the retry holds its permit the erase cannot claim the slot; once the
retry has admitted the session the erase claims it as attached. -/
example : (run cfgEx sHeld [.rRound, .rStart, .rPermit, .eClaim 0]).isNone = true := by decide
/-- Meanwhile the retry is the one build of the session (`one_build`). -/
example : (run cfgEx sHeld [.rRound, .rStart, .rPermit]).any (fun s => building s 0 == 1) = true := by
  decide
example : (run cfgEx sHeld [.rRound, .rStart, .rPermit, .rResult .acquired, .eClaim 0]).any
    (fun s => match s.slots 0 with
      | some (.erasing (some h) .absent) => h.fenced
      | _ => false) = true := by decide

/-- A pinned session whose own heartbeat re-took its lease after an operator
override (#293): the handle is fenced and detached without a release, so the
row stays this process's, at the new token. An in-serve erase of it then runs
as the CLI's eraser identity, is refused as held, and answers 409, which
`EraseAnswer::HeldElsewhere` documents as "a live writer in another
process" (erase.rs:120); it is this one. -/
def zombieRun : List Ev :=
  [.rowLapse 0, .beat 0, .watchFire 0, .lostRun 0 100, .detachRun 0, .eClaim 0, .eEnd 0 false]

example : (run cfgEx s0 zombieRun).any
    (fun s => isHeld (s.slots 0) && s.rows 0 == ⟨.me, 2⟩ && s.rel == []) = true := by decide

/-- And the pinned retry then takes the re-taken row by the refresh arm, at
that same token: a new, healthy handle serves the session again. This is
fine: leases.rs only says the fenced handle's heartbeat stops. -/
example : (run cfgEx s0 [.rowLapse 0, .beat 0, .watchFire 0, .lostRun 0 100, .detachRun 0,
    .strayDrop 100, .tick 5000, .rRound, .rStart, .rPermit, .rResult .acquired]).any
    (fun s => s.rows 0 == ⟨.me, 2⟩ && match s.slots 0 with
      | some (.live h) => h.token == 2 && !h.fenced
      | _ => false) = true := by decide

/-! ### Backoff and the previous handle -/

/-- The pinned session loses its lease and is detached: `HeldElsewhere`, due in
`PINNED_RETRY`, its handle `previous`. -/
def pinLost : List Ev :=
  [.rowLapse 0, .otherTake 0, .beat 0, .watchFire 0, .lostRun 0 100, .detachRun 0]

/-- Not due before `PINNED_RETRY`... -/
example : (run cfgEx s0 (pinLost ++ [.rRound])).any (fun s => s.round == []) = true := by decide

/-- ...due at it; but its old handle is still alive, so the retry pushes
`retry_at` back by `ELECTION_RETRY` and acquires nothing... -/
example : (run cfgEx s0 (pinLost ++ [.tick 5000, .rRound, .rStart, .rPermit])).any
    (fun s => s.slots 0 == some (.held 6000 (some (100, 0))) && s.retry == none &&
      s.builds == []) = true := by decide

/-- ...and once the handle has gone it acquires, and, still held, waits
`PINNED_RETRY` again. -/
example : (run cfgEx s0 (pinLost ++ [.tick 5000, .rRound, .rStart, .rPermit, .strayDrop 100,
    .tick 1000, .rRound, .rStart, .rPermit, .rResult .held])).any
    (fun s => s.slots 0 == some (.held 11000 none) && s.builds == [0]) = true := by decide

/-- On demand, the same wait: a request finds the old handle alive and gets
503; past `PREVIOUS_HANDLE_WAIT` it attaches anyway, and two handles of the
session are alive at once (documented: the `SecondSessionWriter` ERROR the
re-attach logs is then a true report). -/
def leakRun : List Ev :=
  attach1 ++ [.tick 60000, .sweep, .detachRun 1, .start 1 true, .fPermit 1, .tick 30000] ++ attach1

example : (run cfgEx s0 (attach1 ++ [.tick 60000, .sweep, .detachRun 1, .start 1 true, .fPermit 1])).any
    (fun s => s.slots 1 == none && s.builds == [1]) = true := by decide

/-- Past `PREVIOUS_HANDLE_WAIT`, an on-demand request attaches a session whose
old handle is still alive, so two handles of one session are alive at once:
the documented case in which the `SecondSessionWriter` ERROR is a true report.
-/
theorem previous_wait_expires :
    ∃ s, Reach cfgEx s ∧ 0 ∈ s.strays ∧ ∃ h, s.slots 1 = some (.live h) ∧ h.hid = 1 := by
  obtain ⟨s, hs, hp⟩ := reach_of_run (Reach.init s0_init) (es := leakRun)
    (p := fun s => decide (0 ∈ s.strays) && match s.slots 1 with
      | some (.live h) => h.hid == 1
      | _ => false) (by decide)
  refine ⟨s, hs, ?_⟩
  simp only [Bool.and_eq_true, decide_eq_true_eq] at hp
  refine ⟨hp.1, ?_⟩
  revert hp
  cases e : s.slots 1 with
  | none => simp
  | some x => cases x <;> simp

end Runs

end Lambo.Registry
