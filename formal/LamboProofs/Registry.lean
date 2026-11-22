/-
# The multi-session registry of `lambo serve`

Model of the sessions one serve process holds, the tasks that attach and
detach them, and the lease each one's handle holds, as one step relation over
every interleaving of requests and background tasks.

Rust items modelled (paths as at `d58ebcbf`):

* `src/mcp/serve/registry.rs`: `SessionRegistry` and its `Slot`s;
  `route_or_start` (the start and live branches; a wait on an attach in
  flight, a negative-cache answer and every 503 are stutters);
  `attach_on_demand`, `attach_outcome`, `reserve_place`,
  `awaiting_previous_on_demand`, `insert_live_if`, `admit_if`,
  `build_session` and `acquire` (as `fresh` and `acq`); `release_abandoned`;
  `due`, `awaiting_previous`, `retry`, `replace_held`, `retry_loop`;
  `detach`, `spawn_detach`, `run_detach`, `sweep_idle`, `idle_sweeper`;
  `close_set`; `watch_lease`; `answer_for` and `lookup`;
  `LeaseLossPolicy::for_scope`; `PINNED_RETRY`.
* `src/mcp/serve/registry/on_demand.rs`: `Flight` (`end` and its `Drop`),
  `takes_a_place`, `unplaced_cap`, `place_share`, `choose_victim`, `held_by`,
  `NegativeCache::get`, and what `end` caches.
* `src/mcp/serve/registry/erase.rs`: `erase`, `erase_now` (the claim under
  every permit), `erase_attached`, `erase_unattached`, `mark_erased`,
  `after_failed_attached`, `restore`.
* `src/mcp/serve/registry/views.rs`: `slot_views`.
* `src/memory/leases.rs` after #293: `spawn_lease_heartbeat` (`evBeat`),
  `LeaseLostSignal::latch` and `record`, `lease_lost_latched` (the watcher's
  states), `fence_for_erase`, and `close`'s release (once, never when fenced);
  `src/store/flush.rs`'s stale-write latch, which sets the flag alone.
* `src/main.rs`'s `too_many_stdio_sessions`; `http_guards::credential_share`.

## Model

* A session id is a `Nat`; `Cfg.ids` lists every addressable id, the pinned
  ones among them. A slot holds what `Slot` holds; an attaching slot carries
  its `Flight`'s phase (`Phase`), so a flight and its slot are one thing, and
  a detaching slot carries the handle and the flight its eviction keeps
  waiting. A handle (`H`) is a `Memory` with its attached session: its
  fencing token, the lease-lost flag (`fenced`), whether the latch woke its
  waiters (`woken`), its heartbeat, its watcher, its calls in flight and its
  last use.
* The lease rows are the store's, per session, from this process's side:
  `Holder.me` is the process's holder string, which every handle of the
  process shares, so a holder-scoped release cannot tell them apart. A lease
  lapsing at its TTL, an operator override and another writer's release are
  one event (`rowLapse`); another writer's takeover is `otherTake`.
* `Ev` lists the events; `step` runs one, `none` when it is not enabled.
  Each event is a span of Rust that holds the slots' lock, or that runs
  between two `.await`s with nothing else in the registry able to observe the
  difference. Where the model merges spans the Rust separates, the event's
  doc says so: an admission and its `Flight::finish` (no `.await` between
  them; the test-only `panic_after_admit` hook is the pure `endOutcome`); a
  failed attach's permit return, its `release_abandoned` and its `finish`
  (`evFTimeout`); a probe's answer and the reservation; an erase's claim,
  fence and close (the slot reads `Erasing` throughout; the heartbeat is
  marked stopped at the claim, where the Rust's beats on until the close
  aborts it, see `evEClaim`); and `run_detach`'s six stages (the slot reads
  `Detaching` throughout, and the watcher may still fire before it, as before
  stage 5).
* The negative cache is in the state (`neg`): an on-demand attach starts only
  when no fresh entry answers the session. The flight's end caches what
  `Flight::end` caches on the modelled paths (a dropped flight's `Failed`, a
  probe's `Erased` and create-less `Absent`) and an admission forgets the
  entry; a non-transient attach error's `Failed` and `mark_erased`'s `Erased`
  are not recorded, which only lets more attaches start. An entry ignores
  time past `NEGATIVE_TTL`, and `negEvict` drops one early, as a full cache
  drops its oldest (the model does not count entries).
* Choices the Rust leaves to the store, the clock or the scheduler are event
  parameters: a store error, which victim `reserve_place` takes (any one its
  filter allows; `chooseVictim` models which one, with the per-credential
  share), a retry's outcome, whether a load fails. A heartbeat's transient
  error, a negative-cache answer and any other refused request are stutters.
* Times are milliseconds on tokio's clock, as `Nat`; there is no `f64`.
  `saturating_sub` and `saturating_mul` are `Nat` arithmetic, so
  `unplaced_cap(usize::MAX)` is out of scope.

## Results

The invariant `Inv` holds in every reachable state (`reach_inv`, from
`inv_step`, one lemma per event). From it, over every interleaving:

* `mems_le_places`, `live_on_demand_le_places`: **caps under concurrent
  admission**. The on-demand slots holding or about to hold a `Memory` (an
  attach past its eviction wait counts, one still waiting does not: its
  victim does) never exceed `max_attached - pinned`. A detached handle that
  something still holds (a `PreviousHandle`) is not in a slot and is not
  counted (`previous_wait_expires`) (design §3.2 step 3; test
  `eviction_releases_the_lease_and_a_reattach_mints_the_next_token`).
* `attaches_le_permits`: at most `attach_permits` attaches hold a permit
  (`attach_concurrency`). `unplaced_le_cap`: unprobed attaches stay under
  `unplaced_cap` (test `a_flood_of_absent_ids_stays_bounded`).
* `one_build`: **never two concurrent builds for one session**, on-demand or
  pinned (single-flight; test `concurrent_first_requests_attach_once`).
* `erase_claim_quiescent`, `erasing_no_build`: **erase excludes concurrent
  attach**. An erase claims only when no attach of any session holds a
  permit, and while a slot is `Erasing` no acquire of it is in flight (#32; test
  `an_erase_racing_the_pinned_retry_waits_for_it_then_erases_what_it_attached`).
* `retry_admits_over_held`, `fAdmit_own`: `admit_if` never refuses; its
  failure branch is unreachable (the test
  `a_retry_that_finds_its_slot_changed_releases_what_it_took` forces the slot
  "as nothing in the serve now changes it then").

One step at a time:

* `start_needs_absent`: an attach starts only over an absent slot.
* `drop_clears`, `timeout_clears`, `admit_fills_or_releases`: **a reserved
  place is filled or released**: a cancelled, panicking or timed-out attach
  removes its slot and gives its permit back, in any phase; an admission ends
  live or absent (tests
  `an_attach_that_dies_answers_its_waiters_and_frees_its_place`,
  `an_attach_that_hangs_times_out_and_does_not_stick`).
  `tomb_never_admitted`: the tombstone is never attached.
* `retry_waits_previous`, `attach_waits_previous`: **a session awaiting its
  previous handle is not started again** while the handle lives and
  `PREVIOUS_HANDLE_WAIT` has not passed (#32).
  `previous_wait_expires`: past it, it is (documented).
* `mem_due`, `rResult_slot`, `retry_backoff`, `pinnedRetry_ge`: **backoff**.
  `due` takes the held sessions whose `retry_at` has come; every
  `HeldElsewhere` the retry writes after an attempt is `PINNED_RETRY` ahead,
  `awaiting_previous` pushes by `ELECTION_RETRY`, and `PINNED_RETRY` is at
  least a second.
* `sweep_skips_in_flight`, `sweep_never_pinned`, `sweep_takes_idle`: **an
  idle sweep never detaches a session with a call in flight**, nor a pinned
  one (test `the_idle_sweep_skips_a_session_in_use`). `evict_victim`: an
  eviction takes only an idle, unpinned session with nothing in flight.
* `detach_release`, `detach_leaves`: **a detach releases the lease exactly
  once**: an unfenced handle's close releases it once (token kept), a fenced
  one's never; and the handle leaves its slot.
* `write_refused_fenced`, `write_through_live`, `beat_own_gen`,
  `beat_latches`, `stale_not_own_gen`, `stale_quiet`, `parked_needs_wake`,
  `watcher_fires_after_latch`, `erase_fence_not_lost`, `lostRun_any_live`:
  **a lost lease fences the handle, then detaches the entry** (#293). Every
  write through a fenced handle is refused; a beat off the handle's own
  generation latches the fence and the wake-up, and the watcher then fires
  and spawns the detach. The flush loop's stale-write latch sets the flag
  alone: the session stays live, every write refused, until the next beat
  (which always latches after a stale write) wakes the watcher. The erase's
  own fence never reaches the watcher.
* `eClaim_fences`, `eClaim_refuses_busy`, `eEnd_commit`: the erase's claim,
  503s and commit.

Pure pieces: `forScope_exit_iff`, `stdio_exits_on_loss` (the stdio cap and
the one-session policy); `unplacedCap_pos`, `placeShare_pos`,
`placeShare_fair` (shares never add up past the places);
`chooseVictim_eligible`, `chooseVictim_none_iff`, `chooseVictim_max`,
`at_share_evicts_own` (#32); `answer_retry_after_ge`,
`answer_erasing` (#32: 410 waits for the commit), `answer_live_iff`;
`slotViews_pinned_first`, `slotViews_complete`; `negGet_create_not_absent`,
`negGet_expired`, `busy_never_cached`, `panic_after_admit` (#32). Each has
`decide` examples, several of them the Rust's own unit tests.

## What the model shows about the Rust

* `lost_detach_takes_healthy` (with `lostRun_any_live`): `watch_lease`
  spawns `spawn_detach(id, LeaseLost)` by session id (registry.rs:2029), and
  `detach` takes whatever is live under that id (registry.rs:1659-1672). If
  the spawned task runs only after the fenced session was detached by another
  path (the idle sweep, an eviction, a failed erase) and attached again, it
  detaches the new, healthy handle, with its call in flight. Writes stay safe
  (that close flushes and releases); the call is cut and the session detached
  for nothing. It needs the task delayed past a whole detach and attach.
* `dropped_flight_fences_next` (with `stale_release_fences`): `Flight`'s
  `Drop` (on_demand.rs:388-407) removes the slot in `end` and only then
  spawns `release_abandoned`, holder-scoped by the process's holder, whatever
  phase the flight died in. If the session is attached again before that task
  runs, it releases the new handle's lease; the new handle's next beat
  re-takes the row at the next token and, since #293, fences the healthy
  session and drops its tail. In between, another writer could take the
  session while this one accepts writes. It needs a panic or an abort of an
  attach task, and the `Failed` the drop cached (on_demand.rs:377) gone
  before the release runs: evicted from a full negative cache, or the release
  delayed past `NEGATIVE_TTL` (30 s, against a 2 s `LEASE_RELEASE_GRACE`).
  While the entry is fresh, `route_or_start` answers from it and starts
  nothing (registry.rs:675-681). The timeout path does not have this race: it
  releases before `finish`, while the slot is still the flight's.
* `zombieRun`: after a #293 re-take, the fenced handle is detached without a
  release, so the row stays this process's. An in-serve erase then runs as
  the CLI eraser and answers 409 `HeldElsewhere`, documented (erase.rs:120) as
  "a live writer in another process": a doc mismatch, not a safety problem.
* `retry_admits_over_held`: `admit_if`'s refusal branch is dead code under
  the permit discipline.

## What the model leaves out

The MCP transport, the endpoints, rate limits, the layer stack
(`LayerSource`), `LiveSessions` and the process-wide `--max-sessions`
(beyond `credential_share`); logging, the ledger and `book_lease_loss`;
`owners` inside the step relation (the share rule is `chooseVictim`'s);
the negative cache's size (its eviction is the `negEvict` event; `negGet`
and `negPut` model its rules); the `warned` flag; the `started` gate,
`track`, `join_detaches` and `stop_tasks`; the early-shutdown pre-arm and the second-signal escape of
a close; `ERASE_PERMIT_WAIT` and the erase's cut-off (both answer 503 or an
unknown outcome without a state change); an erase whose read-back finds the
tombstone after an error (`Tombstone::Yes` and `Unknown`); the
`ExitProcess` wind-down (a one-session serve has no watcher and no retry
loop); the write-behind log and the flush ladder (`Flush.lean`); how
`build_attach` loads and checks the contract (`Contract.lean`); a
heartbeat's transient errors; and the expiry clock of a lease row, folded
into `rowLapse`. `NegativeCache::put`'s bound is not proved.

## #273's seams

#273 Track 3 proposes `registry/admission.rs`, `registry/retry.rs` and
`registry/lifecycle.rs`. The invariant is one, `inv_step` is its conjunction
over events, and each module would carry the lemmas of its own events:

* `admission.rs` (`route_or_start`, `attach_on_demand`, `attach_outcome`,
  `reserve_place`, `insert_live_if`, `admit_if`, `build_session`):
  `inv_start`, `inv_fPermit`, `inv_fProbe`, `inv_reserve`, `inv_fGo`,
  `inv_fTimeout`, `inv_fDrop`, `inv_fAdmit`, `inv_enter`, `inv_leave` and
  `inv_write` (the routed request's `InFlight`, which `route_or_start`
  takes); `mems_le_places`, `live_on_demand_le_places`,
  `attaches_le_permits`, `unplaced_le_cap`, `one_build`,
  `start_needs_absent`, `fAdmit_own`, `retry_admits_over_held`,
  `evict_victim`, `drop_clears`, `timeout_clears`, `admit_fills_or_releases`,
  `tomb_never_admitted`, `dropped_flight_fences_next`; and, with
  `awaiting_previous_on_demand`, which `attach_outcome` calls,
  `attach_waits_previous` and `previous_wait_expires`. `on_demand.rs` keeps
  `chooseVictim_*`, `at_share_evicts_own`, `unplacedCap_pos`,
  `placeShare_*`, `negGet_*`, `busy_never_cached`, `panic_after_admit`, and
  the negative cache's `negEvict` case of `inv_step`.
* `retry.rs` (`due`, `awaiting_previous`, `retry`, `retry_loop`):
  `inv_rRound`, `inv_rStart`, `inv_rPermit`, `inv_rResult`; `mem_due`,
  `retry_waits_previous`, `rResult_slot`, `retry_backoff`, `pinnedRetry_ge`,
  and the backoff runs.
* `lifecycle.rs` (`release_abandoned`, `run_detach`, `sweep_idle`,
  `idle_sweeper`, and, as argued below, `detach`, `spawn_detach` and
  `watch_lease`): `inv_sweep`, `inv_detachRun`, `inv_wake`, `inv_lostRun`,
  `inv_abandRun`, `inv_strayDrop` (the `PreviousHandle` a detach leaves),
  `inv_watchPark`, `inv_watchFire`; `sweep_skips_in_flight`,
  `sweep_never_pinned`, `sweep_takes_idle`, `detach_release`,
  `detach_leaves`, `lostRun_any_live`, `stale_release_fences`, and the
  `watch_lease` theorems below.
* `registry.rs` keeps the slot table (`Inv` itself), `close_set`
  (`inv_closeSet`, and the `close` case of `inv_step`), `answer_*`,
  `forScope_exit_iff`; `views.rs` `slotViews_*`; `erase.rs` `inv_eClaim`,
  `inv_eEnd`, `erase_claim_quiescent`, `erasing_no_build`, `eClaim_fences`,
  `eClaim_refuses_busy`, `eEnd_commit`, `erase_fence_not_lost`.
* Outside the registry: `inv_beat`, `inv_stale`, `beat_own_gen`,
  `beat_latches`, `stale_not_own_gen`, `stale_quiet` are the heartbeat and the
  flush loop (`memory/leases.rs`, `store/flush.rs`); `inv_rowLapse` and
  `inv_otherTake` are the store and other writers; the `tick` case is time.

**`watch_lease` does not belong in `retry.rs`.** It shares no state with
`due`, `awaiting_previous` or `retry`: it neither reads nor writes a
`HeldElsewhere`, and the retry only sees its effect through `run_detach`'s
last write. Its theorems (`watcher_fires_after_latch`, `parked_needs_wake`,
`stale_quiet`, `beat_latches`, `erase_fence_not_lost`, `lostRun_any_live`,
`lost_detach_takes_healthy`) are about the fence-to-detach path: its trigger
is `memory/leases.rs`'s latch contract as #293 changed it (a `Held` beat, a
re-take at another token, a stale-write flag woken by the next beat) and its
action is `detach`. It belongs with the lease and fencing side of the
registry, in `lifecycle.rs` beside `run_detach` and the sweeper (the other
detach triggers), with its contract stated against `lease_lost_latched`; the
fix for `lost_detach_takes_healthy` (detach only the handle it watched) would
land in `detach` there too, so `detach` and `spawn_detach` go to
`lifecycle.rs` with it.
-/

namespace Lambo.Registry

/-! ## Sums over the session universe -/

/-- `S g l`: the sum of `g` over the list `l`. -/
def S (g : Nat → Nat) : List Nat → Nat
  | [] => 0
  | i :: l => g i + S g l

theorem S_congr {g g' : Nat → Nat} : ∀ {l : List Nat}, (∀ j ∈ l, g' j = g j) → S g' l = S g l
  | [], _ => rfl
  | i :: l, h => by
    simp only [S]
    rw [h i (List.mem_cons_self), S_congr (fun j hj => h j (List.mem_cons_of_mem _ hj))]

theorem S_le {g g' : Nat → Nat} : ∀ {l : List Nat}, (∀ j ∈ l, g' j ≤ g j) → S g' l ≤ S g l
  | [], _ => Nat.le_refl _
  | i :: l, h => by
    simp only [S]
    have h1 := h i List.mem_cons_self
    have h2 := S_le (l := l) (fun j hj => h j (List.mem_cons_of_mem _ hj))
    omega

theorem S_out {g g' : Nat → Nat} {k : Nat} :
    ∀ {l : List Nat}, k ∉ l → (∀ j, j ≠ k → g' j = g j) → S g' l = S g l
  | [], _, _ => rfl
  | i :: l, hk, h => by
    simp only [S]
    have hik : i ≠ k := fun e => hk (e ▸ List.mem_cons_self)
    rw [h i hik, S_out (fun e => hk (List.mem_cons_of_mem _ e)) h]

theorem S_upd {g g' : Nat → Nat} {k : Nat} : ∀ {l : List Nat}, l.Nodup → k ∈ l →
    (∀ j, j ≠ k → g' j = g j) → S g' l + g k = S g l + g' k
  | [], _, hk, _ => absurd hk List.not_mem_nil
  | i :: l, hnd, hk, h => by
    simp only [S]
    rw [List.nodup_cons] at hnd
    by_cases hik : i = k
    · subst hik
      rw [S_out hnd.1 h]
      omega
    · have hkl : k ∈ l := (List.mem_cons.1 hk).resolve_left (fun e => hik e.symm)
      have := S_upd hnd.2 hkl h
      rw [h i hik]
      omega

/-- `upd f k x`: `f` with the value at `k` replaced by `x`. -/
def upd {α : Type} (f : Nat → α) (k : Nat) (x : α) : Nat → α :=
  fun j => if j = k then x else f j

@[simp] theorem upd_same {α : Type} (f : Nat → α) (k : Nat) (x : α) : upd f k x k = x := by
  simp [upd]

theorem upd_ne {α : Type} (f : Nat → α) {k j : Nat} (x : α) (h : j ≠ k) : upd f k x j = f j := by
  simp [upd, h]

/-! ## The lease row, as this process sees it -/

/-- Who holds a session's lease row. `me` is this serve process's holder,
`LeaseHolder::for_this_process(agent)`: every handle the process opens on any
session presents the same holder string, so a holder-scoped release cannot
tell two of its handles apart. `released` is `RELEASED_HOLDER`, or a row whose
lease lapsed at its TTL (both are taken over by the next acquire with the
token incremented). `tomb` is the #23 erasure tombstone. -/
inductive Holder where
  | none
  | me
  | other
  | released
  | tomb
  deriving DecidableEq, Repr

/-- A session's lease row: its holder and fencing token. `none` carries token
`0`, so the first acquire mints `1`, as `Lease.first` does. -/
structure Row where
  holder : Holder
  token : Nat
  deriving DecidableEq, Repr

/-- This process's acquire (`build_attach`'s, or the heartbeat's refresh),
`Lease.acquire` with expiry folded into `released`: the holder's own row is
refreshed and keeps its token, a free row is taken over at the next token,
and a live row of another writer (or the tombstone) is `Held`. -/
def acq (r : Row) : Option Row :=
  match r.holder with
  | .me => some r
  | .none => some ⟨.me, r.token + 1⟩
  | .released => some ⟨.me, r.token + 1⟩
  | .other => none
  | .tomb => none

/-- A release by this process's holder (`GraphStore::release_lease`):
holder-scoped, keeps the token. -/
def relMe (r : Row) : Row :=
  if r.holder = .me then ⟨.released, r.token⟩ else r

/-! ## Handles, slots and the registry state -/

/-- A handle's lease watcher (`watch_lease`, spawned by `insert_live_if` under
`DetachSession`): `unpolled` until its first poll of `lease_lost_latched`,
`parked` once it waits on the wake-up, `fired` once it has booked the loss and
spawned the detach, `off` when there is none (stopped, or `ExitProcess`). -/
inductive Watch where
  | off
  | unpolled
  | parked
  | fired
  deriving DecidableEq, Repr

/-- One `Memory` handle with its attached session's parts. `fenced` is the
`lease_lost` flag; `woken` is a latch of `LeaseLostSignal` that
notified its waiters; `beating` is the heartbeat task; `inflight` and `used`
are `SessionActivity`. Times are milliseconds on tokio's clock. -/
structure H where
  hid : Nat
  token : Nat
  fenced : Bool
  woken : Bool
  beating : Bool
  watch : Watch
  inflight : Nat
  used : Nat
  deriving DecidableEq, Repr

/-- Where an on-demand attach (`attach_on_demand`, its `Flight`) is:
waiting for its permit, holding it while it probes, waiting for an evicted
victim's detach, told the victim is gone, or acquiring and admitting.
`Slot::Attaching { placed }` is `placed = true` exactly from `evicting` on. -/
inductive Phase where
  | queued
  | probing
  | evicting (victim : Nat)
  | ready
  | acquiring
  deriving DecidableEq, Repr

/-- The slot an erase of a session it does not hold replaced, for
`restore`. Only these can be replaced: an erase answers 503 for an attaching,
detaching or erasing slot, and a live one is erased as attached. -/
inductive Prior where
  | absent
  | held (retryAt : Nat) (prev : Option (Nat × Nat))
  | failed
  | erased
  deriving DecidableEq, Repr

/-- `Slot`. `detaching h w` is a detach in flight of handle `h`, with the
flight `w` that evicted it waiting for it, if any. `held r p` is
`HeldElsewhere { retry_at = r, previous = p }`, `p` the detached handle's id
and when it was detached. `erasing h p` is an erase in flight of the attached
handle `h` (already fenced and closed), or of an unattached session whose
slot was `p`. -/
inductive Slot where
  | attaching (create : Bool) (ph : Phase)
  | live (h : H)
  | detaching (h : H) (waiter : Option Nat)
  | held (retryAt : Nat) (prev : Option (Nat × Nat))
  | failed
  | erasing (h : Option H) (prior : Prior)
  | erased
  deriving DecidableEq, Repr

def Prior.slot : Prior → Option Slot
  | .absent => none
  | .held r p => some (.held r p)
  | .failed => some .failed
  | .erased => some .erased

/-- A negative outcome kept in `NegativeCache`. -/
inductive Neg where
  | absent
  | erased
  | failed
  deriving DecidableEq, Repr

/-- `NegativeCache::get`: an entry `(n, at)` answers until `NEGATIVE_TTL`
after `at`; `Absent` never answers a request that may create the session. -/
def negGet (e : Option (Neg × Nat)) (now ttl : Nat) (create : Bool) : Option Neg :=
  match e with
  | none => none
  | some (n, t0) =>
    if t0 + ttl ≤ now then none
    else match n with
      | .absent => if create = true then none else some .absent
      | n => some n

/-- The registry's fixed parameters. `places` is `max_attached - pinned`,
the on-demand places; `permits` is `attach_permits`; `cap` is
`unplaced_cap(max_attached)`; `er`, `pr` and `wait` are `ELECTION_RETRY`,
`PINNED_RETRY` and `PREVIOUS_HANDLE_WAIT`; `idle` and `minIdle` are
`idle_detach` and `min_idle_to_evict`; `watchers` is
`LeaseLossPolicy::DetachSession`; `ttl` is `NEGATIVE_TTL`. `ids` is every session id that can be
addressed, with the pinned ones among them. -/
structure Cfg where
  ids : List Nat
  pinned : List Nat
  onDemand : Bool
  places : Nat
  permits : Nat
  cap : Nat
  er : Nat
  pr : Nat
  wait : Nat
  idle : Nat
  minIdle : Nat
  watchers : Bool
  ttl : Nat

/-- The registry, its tasks and the store's lease rows.

* `free`: the attach permits not held.
* `retry`: the pinned retry loop's current `retry(id)`, `false` while it waits
  for its permit, `true` once it holds it and acquires; `round` the rest of
  the round's `due()` list.
* `lost`: detaches `watch_lease` spawned (`spawn_detach(id, LeaseLost)`), by
  session id, not yet run. `aband`: `release_abandoned` tasks a dropped
  `Flight` spawned, not yet run.
* `strays`: detached handles something may still hold (`PreviousHandle`);
  `prevOD` the on-demand `previous` map; `neg` the `NegativeCache`, each
  entry with the time it was put.
* Ghosts: `next` mints handle ids; `accepted` lists the handles writes went
  through; `rel` the handles whose close released the lease; `builds` the
  sessions an acquire was started for. -/
structure St where
  now : Nat
  slots : Nat → Option Slot
  rows : Nat → Row
  free : Nat
  closing : Bool
  retry : Option (Nat × Bool)
  round : List Nat
  lost : List Nat
  aband : List Nat
  strays : List Nat
  prevOD : Nat → Option (Nat × Nat)
  neg : Nat → Option (Neg × Nat)
  next : Nat
  accepted : List Nat
  rel : List Nat
  builds : List Nat

/-- The handle a slot holds, if any. -/
def Slot.handle : Slot → Option H
  | .live h => some h
  | .detaching h _ => some h
  | .erasing h _ => h
  | _ => none

/-- A freshly admitted handle at token `tok` (`AttachedSession::attach`, then
`insert_live_if`'s watcher under `DetachSession`). -/
def fresh (c : Cfg) (s : St) (tok : Nat) : H :=
  { hid := s.next, token := tok, fenced := false, woken := false, beating := true,
    watch := if c.watchers then .unpolled else .off, inflight := 0, used := s.now }

/-! ## Weights the bounds count -/

def Phase.holds : Phase → Nat
  | .queued => 0
  | _ => 1

def Phase.unplaced : Phase → Nat
  | .queued => 1
  | .probing => 1
  | _ => 0

def Phase.mem : Phase → Nat
  | .ready => 1
  | .acquiring => 1
  | _ => 0

/-- An attach holding one of the `attach_permits`. -/
def holds : Option Slot → Nat
  | some (.attaching _ p) => p.holds
  | _ => 0

/-- `Slot::Attaching { placed: false }`. -/
def unpl : Option Slot → Nat
  | some (.attaching _ p) => p.unplaced
  | _ => 0

/-- `on_demand::takes_a_place`. -/
def place : Option Slot → Nat
  | some (.attaching _ p) => 1 - p.unplaced
  | some (.live _) => 1
  | some (.detaching _ _) => 1
  | some (.erasing _ _) => 1
  | _ => 0

/-- A slot that holds a `Memory`, or whose attach is about to make one: live,
detaching, an erase of an attached handle, or an attach past its eviction
wait. An attach still waiting for its victim's detach is not counted: the
victim's `detaching` slot is. -/
def mem : Option Slot → Nat
  | some (.attaching _ p) => p.mem
  | some (.live _) => 1
  | some (.detaching _ _) => 1
  | some (.erasing (some _) _) => 1
  | _ => 0

def isPinned (c : Cfg) (j : Nat) : Bool := decide (j ∈ c.pinned)

/-- `taken` in `reserve_place`: the on-demand slots that take a place. -/
def taken (c : Cfg) (f : Nat → Option Slot) : Nat :=
  S (fun j => if isPinned c j then 0 else place (f j)) c.ids

/-- The on-demand `Memory`s, held or about to be. -/
def memSum (c : Cfg) (f : Nat → Option Slot) : Nat :=
  S (fun j => if isPinned c j then 0 else mem (f j)) c.ids

def unplSum (c : Cfg) (f : Nat → Option Slot) : Nat :=
  S (fun j => unpl (f j)) c.ids

def retryHolds : Option (Nat × Bool) → Nat
  | some (_, true) => 1
  | _ => 0

/-- Attach permits held: by on-demand attaches and by the pinned retry. -/
def holdSum (c : Cfg) (s : St) : Nat :=
  S (fun j => holds (s.slots j)) c.ids + retryHolds s.retry

/-! ## Events -/

/-- What the pinned retry's attempt came to (`retry`'s match). -/
inductive ROut where
  | acquired
  | held
  | transient
  | erased
  | permanent
  | shutdown
  | timeout
  | abandoned
  deriving DecidableEq, Repr

/-- The events. Each is one atomic step of the Rust: a section under the
slots' lock, or a span between two `.await`s that nothing else in the
registry can observe (each event's definition says which). -/
inductive Ev where
  | tick (d : Nat)
  | start (i : Nat) (create : Bool)
  | enter (i : Nat)
  | leave (i : Nat)
  | write (i : Nat)
  | fPermit (i : Nat)
  | fProbe (i : Nat) (err : Bool) (victim : Option Nat)
  | fGo (i : Nat)
  | fTimeout (i : Nat)
  | fDrop (i : Nat)
  | fAdmit (i : Nat) (ok : Bool)
  | abandRun (i : Nat)
  | rRound
  | rStart
  | rPermit
  | rResult (o : ROut)
  | sweep
  | watchPark (i : Nat)
  | watchFire (i : Nat)
  | lostRun (i : Nat)
  | detachRun (i : Nat)
  | strayDrop (k : Nat)
  | rowLapse (i : Nat)
  | otherTake (i : Nat)
  | beat (i : Nat)
  | stale (i : Nat)
  | eClaim (i : Nat)
  | eEnd (i : Nat) (err : Bool)
  | negEvict (i : Nat)
  | close
  | closeSet
  deriving DecidableEq, Repr

/-- An on-demand flight ends without admission (`Flight::end`): its slot is
removed and the permit it held, if any, goes back. -/
def endFlight (s : St) (i : Nat) (back : Nat) : St :=
  { s with slots := upd s.slots i none, free := s.free + back }

/-- `route_or_start`, the branch that starts an on-demand attach: the slot
is absent, the session is not pinned, the serve attaches on demand and is not
closing, no fresh negative-cache entry answers it (`NegativeCache::get`), and
fewer than `cap` attaches wait unplaced. -/
def evStart (c : Cfg) (s : St) (i : Nat) (cr : Bool) : Option St :=
  if i ∈ c.ids ∧ isPinned c i = false ∧ c.onDemand = true ∧ s.slots i = none ∧
      s.closing = false ∧ unplSum c s.slots < c.cap ∧ negGet (s.neg i) s.now c.ttl cr = none then
    some { s with slots := upd s.slots i (some (.attaching cr .queued)) }
  else none

/-- `route_or_start`'s live branch: the request is in flight from here
(`activity.enter()` under the slots' lock), which stamps the use. -/
def evEnter (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.live h) =>
    some { s with slots := upd s.slots i (some (.live { h with inflight := h.inflight + 1,
                                                                 used := s.now })) }
  | _ => none

/-- A routed request's `InFlight` drops: the end of a call is a use. -/
def evLeave (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.live h) =>
    if h.inflight = 0 then none
    else some { s with slots := upd s.slots i (some (.live { h with inflight := h.inflight - 1,
                                                                      used := s.now })) }
  | _ => none

/-- A tool call in flight writes through the session's handle. The `Memory`
write gate refuses it on a fenced handle (`lease_lost_error`), so the event
exists only for an unfenced one. -/
def evWrite (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.live h) =>
    if h.inflight = 0 ∨ h.fenced = true then none
    else some { s with accepted := h.hid :: s.accepted }
  | _ => none

/-- `awaiting_previous_on_demand`: the session's last detached handle is
still alive and its wait is not over. -/
def awaitingOD (c : Cfg) (s : St) (i : Nat) : Bool :=
  match s.prevOD i with
  | some (k, t0) => decide (k ∈ s.strays) && decide (s.now < t0 + c.wait)
  | none => false

/-- `attach_outcome` up to its probe: the permit (raced against the shutdown,
biased to it), the closing check and `awaiting_previous_on_demand`. A dead
or expired previous handle is forgotten. -/
def evFPermit (c : Cfg) (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.attaching cr .queued) =>
    if s.closing = true then some (endFlight s i 0)
    else if s.free = 0 then none
    else if awaitingOD c s i = true then some (endFlight s i 0)
    else some { s with slots := upd s.slots i (some (.attaching cr .probing)),
                       free := s.free - 1, prevOD := upd s.prevOD i none }
  | _ => none

/-- A victim `reserve_place` may evict for flight `i` (`choose_victim`'s
filter, without its per-credential share, which `chooseVictim` below
models): live, on demand, no call in flight, idle at least `minIdle`. -/
def evictable (c : Cfg) (s : St) (v : Nat) : Option H :=
  match s.slots v with
  | some (.live hv) =>
    if isPinned c v = false ∧ hv.inflight = 0 ∧ hv.used + c.minIdle ≤ s.now then some hv
    else none
  | _ => none

/-- `reserve_place` for flight `i`: a free place, or the victim `victim`
(set `Detaching`, its detach spawned with `i` waiting on it), or `Full`.
Which eligible victim is taken, and whether `Full` is answered although one
exists, is left open (`choose_victim`'s choice is modelled separately). -/
def reserve (c : Cfg) (s : St) (i : Nat) (cr : Bool) (victim : Option Nat) : Option St :=
  if taken c s.slots < c.places then
    some { s with slots := upd s.slots i (some (.attaching cr .acquiring)), builds := i :: s.builds }
  else
    match victim with
    | none => some (endFlight s i 1)
    | some v =>
      match evictable c s v with
      | some hv =>
        some { s with slots := upd (upd s.slots v (some (.detaching hv (some i)))) i
                                    (some (.attaching cr (.evicting v))) }
      | none => none

/-- `attach_outcome` from the probe to the start of the acquire: the probe
(raced against the shutdown), the probe's verdict on the lease row, and
`reserve_place`, with no `.await` between the probe's answer and the
reservation. A store error or timeout is `err`. A free place goes straight on
to the acquire. An erased session, and an absent one the flight may not
create, are cached (`Flight::end`); held elsewhere and a store error are not. -/
def evFProbe (c : Cfg) (s : St) (i : Nat) (err : Bool) (victim : Option Nat) : Option St :=
  match s.slots i with
  | some (.attaching cr .probing) =>
    if s.closing = true ∨ err = true then some (endFlight s i 1)
    else
      match (s.rows i).holder with
      | .tomb => some { endFlight s i 1 with neg := upd s.neg i (some (.erased, s.now)) }
      | .other => some (endFlight s i 1)
      | .none =>
        if cr = true then reserve c s i cr victim
        else some { endFlight s i 1 with neg := upd s.neg i (some (.absent, s.now)) }
      | _ => reserve c s i cr victim
  | _ => none

/-- The flight's wait for its victim's detach (raced against the shutdown,
biased to it), then the start of the acquire, raced the same way: abandoned
there, `release_abandoned` runs while the slot is still the flight's. -/
def evFGo (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.attaching _ (.evicting _)) =>
    if s.closing = true then some (endFlight s i 1) else none
  | some (.attaching cr .ready) =>
    if s.closing = true then
      some { endFlight s i 1 with rows := upd s.rows i (relMe (s.rows i)) }
    else some { s with slots := upd s.slots i (some (.attaching cr .acquiring)),
                       builds := i :: s.builds }
  | _ => none

/-- `attach_on_demand`'s `ATTACH_TIMEOUT` (any phase), or the shutdown winning
the acquire's race: the future is dropped with its permit, and when the
acquire had started the lease it may have taken is released, holder-scoped,
before `Flight::finish` removes the slot.

One step merges three the Rust separates: the permit goes back when
`attach_outcome`'s future is dropped, then `release_abandoned` is awaited
(registry.rs:752-760) with the permit free and the slot still `Attaching`,
then `finish`. In between, an erase may claim another session (the permits
are free), an erase of this one answers 503 (its slot is `Attaching`), and no
attach of this one starts (the slot is taken); the flight never admits. So
nothing the merge hides changes what any theorem here says. -/
def evFTimeout (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.attaching _ p) =>
    let s' := endFlight s i p.holds
    if p = .acquiring then some { s' with rows := upd s.rows i (relMe (s.rows i)) }
    else some s'
  | _ => none

/-- The attach task panics or is aborted (`Flight`'s `Drop`): `end(Failed)`
removes the slot (it is still the flight's), the permit goes with the future,
`Failed` is cached (the slot was still the flight's, on_demand.rs:377), and a
`release_abandoned` task is spawned, whatever phase the flight was in. -/
def evFDrop (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.attaching _ p) =>
    some { endFlight s i p.holds with aband := i :: s.aband,
                                      neg := upd s.neg i (some (.failed, s.now)) }
  | _ => none

/-- `build_attach` and `admit_if` over the flight's own slot, which no
`.await` separates from `Flight::finish`. Held by another live writer or the
tombstone: the flight ends. Otherwise the acquire takes the row; a load or
contract error then releases what it took (`ok = false`). An admission
forgets the session's negative entry. A non-transient error is cached by the
Rust as `Failed`; the model does not record it, which only lets more attaches
start. -/
def evFAdmit (c : Cfg) (s : St) (i : Nat) (ok : Bool) : Option St :=
  match s.slots i with
  | some (.attaching _ .acquiring) =>
    match acq (s.rows i) with
    | none => some (endFlight s i 1)
    | some r =>
      if ok = true then
        some { s with slots := upd s.slots i (some (.live (fresh c s r.token))),
                      rows := upd s.rows i r, free := s.free + 1, next := s.next + 1,
                      neg := upd s.neg i none }
      else some { endFlight s i 1 with rows := upd s.rows i (relMe r) }
  | _ => none

/-- A `release_abandoned` a dropped flight spawned runs: holder-scoped, so it
releases whatever row this process holds on the session by then. -/
def evAbandRun (s : St) (i : Nat) : Option St :=
  if i ∈ s.aband then
    some { s with aband := s.aband.erase i, rows := upd s.rows i (relMe (s.rows i)) }
  else none

/-- `due()`: the pinned sessions held elsewhere whose `retry_at` has come, in
pinned order. -/
def due (c : Cfg) (s : St) : List Nat :=
  c.pinned.filter (fun j => match s.slots j with
    | some (.held r _) => decide (r ≤ s.now)
    | _ => false)

/-- `retry_loop`, one round: its sleep is time passing; then `due()`. -/
def evRRound (c : Cfg) (s : St) : Option St :=
  if s.retry = none ∧ s.round = [] ∧ s.closing = false then some { s with round := due c s }
  else none

/-- The round calls `retry(id)` for its next due session. -/
def evRStart (s : St) : Option St :=
  match s.retry, s.round with
  | none, j :: rest => some { s with retry := some (j, false), round := rest }
  | _, _ => none

/-- `retry` up to its acquire: the permit (biased to the shutdown), the
`HeldElsewhere` check, the closing check and `awaiting_previous`, which pushes
`retry_at` back by `ELECTION_RETRY` while the previous handle is alive and its
wait is not over, and forgets it otherwise. -/
def evRPermit (c : Cfg) (s : St) : Option St :=
  match s.retry with
  | some (i, false) =>
    if s.closing = true then some { s with retry := none }
    else if s.free = 0 then none
    else
      match s.slots i with
      | some (.held r p) =>
        let go : St := { s with slots := upd s.slots i (some (.held r none)),
                                retry := some (i, true), free := s.free - 1,
                                builds := i :: s.builds }
        match p with
        | some (k, t0) =>
          if k ∈ s.strays ∧ s.now < t0 + c.wait then
            some { s with slots := upd s.slots i (some (.held (s.now + c.er) p)), retry := none }
          else some go
        | none => some go
      | _ => some { s with retry := none }
  | _ => none

/-- `replace_held`: the retry's last write, only over a `HeldElsewhere`. -/
def replaceHeld (f : Nat → Option Slot) (i : Nat) (x : Slot) : Nat → Option Slot :=
  match f i with
  | some (.held _ _) => upd f i (some x)
  | _ => f

def isHeld : Option Slot → Bool
  | some (.held _ _) => true
  | _ => false

/-- The retry is over and gives its permit back. -/
def retryDone (s : St) : St := { s with retry := none, free := s.free + 1 }

/-- `retry` from its acquire on. `acquired`: `admit_if` over a
`HeldElsewhere` slot, else the attach is closed and its lease released.
`held`, `transient`, `timeout`: tried again in `PINNED_RETRY` (a timeout
releases first). `erased` (the tombstone read back) and `permanent`: the slot
is `Erased` or `Failed`. `shutdown`: a failure while the process exits leaves
the slot. `abandoned`: the shutdown won the race; the lease is released. -/
def evRResult (c : Cfg) (s : St) (o : ROut) : Option St :=
  match s.retry with
  | some (i, true) =>
    match o with
    | .acquired =>
      match acq (s.rows i) with
      | some r =>
        if isHeld (s.slots i) = true then
          some { retryDone s with slots := upd s.slots i (some (.live (fresh c s r.token))),
                                  rows := upd s.rows i r, next := s.next + 1 }
        else some { retryDone s with rows := upd s.rows i (relMe r), next := s.next + 1,
                                     rel := s.next :: s.rel }
      | none => none
    | .held =>
      if (s.rows i).holder = .other then
        some { retryDone s with slots := replaceHeld s.slots i (.held (s.now + c.pr) none) }
      else none
    | .transient => some { retryDone s with slots := replaceHeld s.slots i (.held (s.now + c.pr) none) }
    | .erased =>
      if (s.rows i).holder = .tomb then some { retryDone s with slots := replaceHeld s.slots i .erased }
      else none
    | .permanent =>
      if (s.rows i).holder = .tomb then none
      else some { retryDone s with slots := replaceHeld s.slots i .failed }
    | .shutdown => some (retryDone s)
    | .timeout =>
      some { retryDone s with slots := replaceHeld s.slots i (.held (s.now + c.pr) none),
                              rows := upd s.rows i (relMe (s.rows i)) }
    | .abandoned => some { retryDone s with rows := upd s.rows i (relMe (s.rows i)) }
  | _ => none

/-- `sweep_idle` on one slot: an on-demand live session with nothing in
flight, idle at least `idle`, is set `Detaching`. -/
def sweepSlot (c : Cfg) (now : Nat) (j : Nat) (x : Option Slot) : Option Slot :=
  match x with
  | some (.live h) =>
    if isPinned c j = false ∧ h.inflight = 0 ∧ h.used + c.idle ≤ now then
      some (.detaching h none)
    else x
  | _ => x

/-- One round of the idle sweeper, under the slots' lock, not when closing. -/
def evSweep (c : Cfg) (s : St) : Option St :=
  if c.onDemand = true ∧ s.closing = false then
    some { s with slots := fun j => sweepSlot c s.now j (s.slots j) }
  else none

/-- Apply `f` to the handle of a live or detaching slot. -/
def mapLD (x : Option Slot) (f : H → H) : Option Slot :=
  match x with
  | some (.live h) => some (.live (f h))
  | some (.detaching h w) => some (.detaching (f h) w)
  | _ => x

/-- The handle of a live or detaching slot (its tasks still run there; a
detach stops them at its stage 5). -/
def ldHandle : Option Slot → Option H
  | some (.live h) => some h
  | some (.detaching h _) => some h
  | _ => none

/-- `watch_lease`'s first poll of `lease_lost_latched` finds the flag clear
and parks on the wake-up. -/
def evWatchPark (s : St) (i : Nat) : Option St :=
  match ldHandle (s.slots i) with
  | some h =>
    if h.watch = .unpolled ∧ h.fenced = false then
      some { s with slots := upd s.slots i (mapLD (s.slots i) (fun h => { h with watch := .parked })) }
    else none
  | none => none

/-- `watch_lease` returns from `lease_lost_latched`: on its first poll with
the flag set, or woken while parked. It books the loss and spawns
`spawn_detach(id, LeaseLost)`, by session id. -/
def evWatchFire (s : St) (i : Nat) : Option St :=
  match ldHandle (s.slots i) with
  | some h =>
    if (h.watch = .unpolled ∧ h.fenced = true) ∨ (h.watch = .parked ∧ h.woken = true) then
      some { s with slots := upd s.slots i (mapLD (s.slots i) (fun h => { h with watch := .fired })),
                    lost := i :: s.lost }
    else none
  | none => none

/-- The spawned `detach(id, LeaseLost)` runs: whatever session is live in the
slot is set `Detaching`; any other state is left. -/
def evLostRun (s : St) (i : Nat) : Option St :=
  if i ∈ s.lost then
    let s' := { s with lost := s.lost.erase i }
    match s.slots i with
    | some (.live h) => some { s' with slots := upd s.slots i (some (.detaching h none)) }
    | _ => some s'
  else none

/-- The flight waiting on a detach of `i` is told the victim is gone (the
oneshot the detach task sends when `run_detach` returns). -/
def wake (f : Nat → Option Slot) (i : Nat) : Option Nat → Nat → Option Slot
  | none => f
  | some w =>
    match f w with
    | some (.attaching cr (.evicting v)) => if v = i then upd f w (some (.attaching cr .ready)) else f
    | _ => f

/-- `run_detach`: the close (an unfenced handle flushes and releases its
lease; a fenced one does neither), its tasks stopped, its endpoint released,
then the slot: `HeldElsewhere` with the previous handle for a pinned
session, removed (and the handle kept in `previous`) for an on-demand one;
then the evicting flight, if any, is told. -/
def evDetachRun (c : Cfg) (s : St) (i : Nat) : Option St :=
  match s.slots i with
  | some (.detaching h w) =>
    let rows' := if h.fenced = true then s.rows else upd s.rows i (relMe (s.rows i))
    let rel' := if h.fenced = true then s.rel else h.hid :: s.rel
    if isPinned c i = true then
      some { s with slots := wake (upd s.slots i (some (.held (s.now + c.pr) (some (h.hid, s.now))))) i w,
                    rows := rows', rel := rel', strays := h.hid :: s.strays }
    else
      some { s with slots := wake (upd s.slots i none) i w, rows := rows', rel := rel',
                    strays := h.hid :: s.strays, prevOD := upd s.prevOD i (some (h.hid, s.now)) }
  | _ => none

/-- The last owner of a detached handle lets go of it. -/
def evStrayDrop (s : St) (k : Nat) : Option St :=
  if k ∈ s.strays then some { s with strays := s.strays.erase k } else none

/-- A lease row lapses at its TTL, or is released by its holder, or by the
operator override: the token is kept. -/
def evRowLapse (s : St) (i : Nat) : Option St :=
  match (s.rows i).holder with
  | .me => some { s with rows := upd s.rows i ⟨.released, (s.rows i).token⟩ }
  | .other => some { s with rows := upd s.rows i ⟨.released, (s.rows i).token⟩ }
  | _ => none

/-- Another process takes a free row at the next token. -/
def evOtherTake (s : St) (i : Nat) : Option St :=
  match (s.rows i).holder with
  | .none => some { s with rows := upd s.rows i ⟨.other, (s.rows i).token + 1⟩ }
  | .released => some { s with rows := upd s.rows i ⟨.other, (s.rows i).token + 1⟩ }
  | _ => none

/-- Whether a refresh (an acquire by this holder) leaves the handle's own
generation: the row is this process's at the handle's token. -/
def ownGen (r : Row) (h : H) : Bool := decide (r.holder = .me) && decide (r.token = h.token)

/-- `spawn_lease_heartbeat` after #293, one beat of a running heartbeat. On
the handle's own generation the refresh changes nothing. Otherwise it
latches: the fence, then the wake-up, then the heartbeat stops. A free row
is re-taken at the next token first (`Acquired` at a token other than the
handle's), a live row of another writer or the tombstone is `Held`. -/
def evBeat (s : St) (i : Nat) : Option St :=
  match ldHandle (s.slots i) with
  | some h =>
    if h.beating = false then none
    else if ownGen (s.rows i) h = true then some s
    else
      let r := s.rows i
      let r' : Row := match r.holder with
        | .none => ⟨.me, r.token + 1⟩
        | .released => ⟨.me, r.token + 1⟩
        | _ => r
      some { s with
        slots := upd s.slots i (mapLD (s.slots i)
          (fun h => { h with fenced := true, woken := true, beating := false })),
        rows := upd s.rows i r' }
  | none => none

/-- The flush loop's `StaleWrite` (#293): the store refuses the handle's
token, and the loop latches the shared flag alone, without the wake-up. -/
def evStale (s : St) (i : Nat) : Option St :=
  match ldHandle (s.slots i) with
  | some h =>
    if h.token < (s.rows i).token ∨ (s.rows i).holder = .tomb then
      some { s with slots := upd s.slots i (mapLD (s.slots i) (fun h => { h with fenced := true })) }
    else none
  | none => none

/-- `erase_now` from its permits to the erase itself: every attach permit
(none held by an attach), the closing check, the claim, and for an attached
session the watcher stopped, the quiet fence and the close, which on a fenced
handle neither flushes nor releases. An attaching, detaching or erasing slot
is answered 503 with nothing changed (a stutter, not an event).

The heartbeat is marked stopped at the claim. In the Rust it runs on until
the close aborts it, through the MCP drain (up to `SHUTDOWN_GRACE`); a beat
there refreshes the row this handle still holds, or latches a fence that is
already latched, and its watcher is already stopped, so the model leaves
those beats out. -/
def evEClaim (c : Cfg) (s : St) (i : Nat) : Option St :=
  if i ∈ c.ids ∧ s.free = c.permits ∧ s.closing = false then
    match s.slots i with
    | some (.live h) =>
      let h' : H := { h with fenced := true, watch := .off, beating := false }
      some { s with slots := upd s.slots i (some (.erasing (some h') .absent)) }
    | some (.held r p) => some { s with slots := upd s.slots i (some (.erasing none (.held r p))) }
    | some .failed => some { s with slots := upd s.slots i (some (.erasing none .failed)) }
    | some .erased => some { s with slots := upd s.slots i (some (.erasing none .erased)) }
    | none => some { s with slots := upd s.slots i (some (.erasing none .absent)) }
    | _ => none
  else none

/-- `mark_erased`: `Erased` for a pinned session, the slot removed for an
on-demand one. -/
def markErased (c : Cfg) (f : Nat → Option Slot) (i : Nat) : Nat → Option Slot :=
  if isPinned c i = true then upd f i (some .erased) else upd f i none

/-- `after_failed_attached`: an attached session whose erase did not commit.
A pinned one is retried in the background (after its old handle goes), an
on-demand one loses its slot and keeps its handle in `previous`. -/
def afterFailed (c : Cfg) (s : St) (i : Nat) (h : H) : St :=
  if isPinned c i = true then
    { s with slots := upd s.slots i (some (.held (s.now + c.pr) (some (h.hid, s.now)))),
             strays := h.hid :: s.strays }
  else
    { s with slots := upd s.slots i none, strays := h.hid :: s.strays,
             prevOD := upd s.prevOD i (some (h.hid, s.now)) }

/-- The store erase and its answer. Attached: as this process's holder; a
live row of another writer is `Held`, and a store error that did not commit
releases the lease; either way `after_failed_attached`. Unattached: as the
CLI's eraser identity, so any live row is `Held` (this process's own
included), and a failure puts the prior slot back (`restore`). A commit
writes the tombstone; a tombstone already there is `already_absent`. -/
def evEEnd (c : Cfg) (s : St) (i : Nat) (err : Bool) : Option St :=
  match s.slots i with
  | some (.erasing (some h) _) =>
    match (s.rows i).holder with
    | .other => some (afterFailed c s i h)
    | .tomb => some { s with slots := markErased c s.slots i }
    | _ =>
      if err = true then
        some (afterFailed c ({ s with rows := upd s.rows i (relMe (s.rows i)), rel := h.hid :: s.rel }) i h)
      else some { s with slots := markErased c s.slots i, rows := upd s.rows i ⟨.tomb, (s.rows i).token + 1⟩ }
  | some (.erasing none p) =>
    match (s.rows i).holder with
    | .me => some { s with slots := upd s.slots i p.slot }
    | .other => some { s with slots := upd s.slots i p.slot }
    | .tomb => some { s with slots := markErased c s.slots i }
    | _ =>
      if err = true then some { s with slots := upd s.slots i p.slot }
      else some { s with slots := markErased c s.slots i, rows := upd s.rows i ⟨.tomb, (s.rows i).token + 1⟩ }
  | _ => none

/-- A negative entry leaves the cache before its `NEGATIVE_TTL` is up:
`NegativeCache::put` drops the oldest entry when it holds
`NEGATIVE_CACHE_MAX`. The model does not count entries, so this is enabled
for any entry; in the Rust it needs a full cache with this entry the oldest.
(An entry past its TTL is ignored by `negGet` without this event.) -/
def evNegEvict (s : St) (i : Nat) : Option St :=
  if (s.neg i).isSome then some { s with neg := upd s.neg i none } else none

/-- `close_set`'s first line: no attach or sweep starts from here. -/
def evClose (s : St) : Option St := some { s with closing := true }

def liveUnfenced : Option Slot → Option Nat
  | some (.live h) => if h.fenced = true then none else some h.hid
  | _ => none

/-- `close_set` once it holds every permit: the live sessions leave their
slots for the shutdown's close, which releases each unfenced one's lease. -/
def evCloseSet (c : Cfg) (s : St) : Option St :=
  if s.closing = true ∧ s.free = c.permits then
    some { s with
      slots := fun j => match s.slots j with
        | some (.live _) => none
        | x => x,
      rows := fun j => match liveUnfenced (s.slots j) with
        | some _ => relMe (s.rows j)
        | none => s.rows j,
      rel := c.ids.filterMap (fun j => liveUnfenced (s.slots j)) ++ s.rel }
  else none

/-- One step of the registry. `none`: the event is not enabled. -/
def step (c : Cfg) (s : St) : Ev → Option St
  | .tick d => some { s with now := s.now + d }
  | .start i cr => evStart c s i cr
  | .enter i => evEnter s i
  | .leave i => evLeave s i
  | .write i => evWrite s i
  | .fPermit i => evFPermit c s i
  | .fProbe i err v => evFProbe c s i err v
  | .fGo i => evFGo s i
  | .fTimeout i => evFTimeout s i
  | .fDrop i => evFDrop s i
  | .fAdmit i ok => evFAdmit c s i ok
  | .abandRun i => evAbandRun s i
  | .rRound => evRRound c s
  | .rStart => evRStart s
  | .rPermit => evRPermit c s
  | .rResult o => evRResult c s o
  | .sweep => evSweep c s
  | .watchPark i => evWatchPark s i
  | .watchFire i => evWatchFire s i
  | .lostRun i => evLostRun s i
  | .detachRun i => evDetachRun c s i
  | .strayDrop k => evStrayDrop s k
  | .rowLapse i => evRowLapse s i
  | .otherTake i => evOtherTake s i
  | .beat i => evBeat s i
  | .stale i => evStale s i
  | .eClaim i => evEClaim c s i
  | .eEnd i err => evEEnd c s i err
  | .negEvict i => evNegEvict s i
  | .close => evClose s
  | .closeSet => evCloseSet c s

/-- Run a list of events; `none` as soon as one is not enabled. -/
def run (c : Cfg) (s : St) : List Ev → Option St
  | [] => some s
  | e :: es =>
    match step c s e with
    | some s' => run c s' es
    | none => none

/-! ### Weights of each slot form -/

@[simp] theorem holds_none : holds none = 0 := rfl
@[simp] theorem holds_att {cr : Bool} {p : Phase} : holds (some (.attaching cr p)) = p.holds := rfl
@[simp] theorem holds_live {h : H} : holds (some (.live h)) = 0 := rfl
@[simp] theorem holds_det {h : H} {w : Option Nat} : holds (some (.detaching h w)) = 0 := rfl
@[simp] theorem holds_held {r : Nat} {q : Option (Nat × Nat)} : holds (some (.held r q)) = 0 := rfl
@[simp] theorem holds_failed : holds (some .failed) = 0 := rfl
@[simp] theorem holds_ers {h : H} {q : Prior} : holds (some (.erasing (some h) q)) = 0 := rfl
@[simp] theorem holds_ern {q : Prior} : holds (some (.erasing none q)) = 0 := rfl
@[simp] theorem holds_erased : holds (some .erased) = 0 := rfl

@[simp] theorem unpl_none : unpl none = 0 := rfl
@[simp] theorem unpl_att {cr : Bool} {p : Phase} : unpl (some (.attaching cr p)) = p.unplaced := rfl
@[simp] theorem unpl_live {h : H} : unpl (some (.live h)) = 0 := rfl
@[simp] theorem unpl_det {h : H} {w : Option Nat} : unpl (some (.detaching h w)) = 0 := rfl
@[simp] theorem unpl_held {r : Nat} {q : Option (Nat × Nat)} : unpl (some (.held r q)) = 0 := rfl
@[simp] theorem unpl_failed : unpl (some .failed) = 0 := rfl
@[simp] theorem unpl_ers {h : H} {q : Prior} : unpl (some (.erasing (some h) q)) = 0 := rfl
@[simp] theorem unpl_ern {q : Prior} : unpl (some (.erasing none q)) = 0 := rfl
@[simp] theorem unpl_erased : unpl (some .erased) = 0 := rfl

@[simp] theorem place_none : place none = 0 := rfl
@[simp] theorem place_att {cr : Bool} {p : Phase} : place (some (.attaching cr p)) = 1 - p.unplaced := rfl
@[simp] theorem place_live {h : H} : place (some (.live h)) = 1 := rfl
@[simp] theorem place_det {h : H} {w : Option Nat} : place (some (.detaching h w)) = 1 := rfl
@[simp] theorem place_held {r : Nat} {q : Option (Nat × Nat)} : place (some (.held r q)) = 0 := rfl
@[simp] theorem place_failed : place (some .failed) = 0 := rfl
@[simp] theorem place_ers {h : H} {q : Prior} : place (some (.erasing (some h) q)) = 1 := rfl
@[simp] theorem place_ern {q : Prior} : place (some (.erasing none q)) = 1 := rfl
@[simp] theorem place_erased : place (some .erased) = 0 := rfl

@[simp] theorem mem_none : mem none = 0 := rfl
@[simp] theorem mem_att {cr : Bool} {p : Phase} : mem (some (.attaching cr p)) = p.mem := rfl
@[simp] theorem mem_live {h : H} : mem (some (.live h)) = 1 := rfl
@[simp] theorem mem_det {h : H} {w : Option Nat} : mem (some (.detaching h w)) = 1 := rfl
@[simp] theorem mem_held {r : Nat} {q : Option (Nat × Nat)} : mem (some (.held r q)) = 0 := rfl
@[simp] theorem mem_failed : mem (some .failed) = 0 := rfl
@[simp] theorem mem_ers {h : H} {q : Prior} : mem (some (.erasing (some h) q)) = 1 := rfl
@[simp] theorem mem_ern {q : Prior} : mem (some (.erasing none q)) = 0 := rfl
@[simp] theorem mem_erased : mem (some .erased) = 0 := rfl

@[simp] theorem isHeld_none : isHeld none = false := rfl
@[simp] theorem isHeld_att {cr : Bool} {p : Phase} : isHeld (some (.attaching cr p)) = false := rfl
@[simp] theorem isHeld_live {h : H} : isHeld (some (.live h)) = false := rfl
@[simp] theorem isHeld_det {h : H} {w : Option Nat} : isHeld (some (.detaching h w)) = false := rfl
@[simp] theorem isHeld_held {r : Nat} {q : Option (Nat × Nat)} : isHeld (some (.held r q)) = true := rfl
@[simp] theorem isHeld_failed : isHeld (some .failed) = false := rfl
@[simp] theorem isHeld_ers {h : H} {q : Prior} : isHeld (some (.erasing (some h) q)) = false := rfl
@[simp] theorem isHeld_ern {q : Prior} : isHeld (some (.erasing none q)) = false := rfl
@[simp] theorem isHeld_erased : isHeld (some .erased) = false := rfl


@[simp] theorem holds_erasing {h : Option H} {q : Prior} : holds (some (.erasing h q)) = 0 := rfl
@[simp] theorem unpl_erasing {h : Option H} {q : Prior} : unpl (some (.erasing h q)) = 0 := rfl
@[simp] theorem place_erasing {h : Option H} {q : Prior} : place (some (.erasing h q)) = 1 := rfl
@[simp] theorem isHeld_erasing {h : Option H} {q : Prior} :
    isHeld (some (.erasing h q)) = false := rfl

/-! ## The invariant -/

/-- The configuration is well formed: the ids are distinct and include the
pinned ones (`[serve]` refuses more pinned sessions than `max_attached`, and
`places` is what is left). -/
structure WF (c : Cfg) : Prop where
  nodup : c.ids.Nodup
  pinned : ∀ j ∈ c.pinned, j ∈ c.ids

/-- Slots only an on-demand session can be in: an attach in flight, and a
detach an attach waits on (an eviction). -/
def odOnly : Option Slot → Bool
  | some (.attaching _ _) => true
  | some (.detaching _ (some _)) => true
  | _ => false

/-- What holds in every reachable state. -/
structure Inv (c : Cfg) (s : St) : Prop where
  out : ∀ j, j ∉ c.ids → s.slots j = none
  od : ∀ j, odOnly (s.slots j) = true → isPinned c j = false
  retryPin : ∀ j b, s.retry = some (j, b) → j ∈ c.pinned
  roundPin : ∀ j ∈ s.round, j ∈ c.pinned
  retryHeld : ∀ j, s.retry = some (j, true) → isHeld (s.slots j) = true
  permits : s.free + holdSum c s = c.permits
  mems : memSum c s.slots ≤ c.places
  unplaced : unplSum c s.slots ≤ c.cap

theorem Inv.attPin {c : Cfg} {s : St} (hI : Inv c s) {j : Nat} {cr : Bool} {p : Phase}
    (h : s.slots j = some (.attaching cr p)) : isPinned c j = false :=
  hI.od j (by rw [h]; rfl)

/-! ### Updating one slot -/

theorem sum_upd (w : Nat → Option Slot → Nat) {ids : List Nat} (hnd : ids.Nodup) {k : Nat}
    (hk : k ∈ ids) (f : Nat → Option Slot) (x : Option Slot) :
    S (fun j => w j (upd f k x j)) ids + w k (f k) = S (fun j => w j (f j)) ids + w k x := by
  have := S_upd (g := fun j => w j (f j)) (g' := fun j => w j (upd f k x j)) hnd hk
    (fun j hj => by simp [upd_ne _ _ hj])
  simpa using this

theorem mem_of_some {c : Cfg} {s : St} (hI : Inv c s) {k : Nat} {x : Slot}
    (h : s.slots k = some x) : k ∈ c.ids := by
  by_cases hk : k ∈ c.ids
  · exact hk
  · rw [hI.out k hk] at h
    cases h

/-- The sums after replacing slot `k` by `x`. -/
theorem sums_upd {c : Cfg} (hc : WF c) {k : Nat} (hk : k ∈ c.ids) (f : Nat → Option Slot)
    (x : Option Slot) :
    S (fun j => holds (upd f k x j)) c.ids + holds (f k) = S (fun j => holds (f j)) c.ids + holds x ∧
    memSum c (upd f k x) + (if isPinned c k then 0 else mem (f k)) =
      memSum c f + (if isPinned c k then 0 else mem x) ∧
    unplSum c (upd f k x) + unpl (f k) = unplSum c f + unpl x ∧
    taken c (upd f k x) + (if isPinned c k then 0 else place (f k)) =
      taken c f + (if isPinned c k then 0 else place x) :=
  ⟨sum_upd (fun _ y => holds y) hc.nodup hk f x,
   sum_upd (fun j y => if isPinned c j then 0 else mem y) hc.nodup hk f x,
   sum_upd (fun _ y => unpl y) hc.nodup hk f x,
   sum_upd (fun j y => if isPinned c j then 0 else place y) hc.nodup hk f x⟩

theorem mem_le_place (x : Option Slot) : mem x ≤ place x := by
  match x with
  | none => simp
  | some (.attaching _ p) => cases p <;> simp [Phase.mem, Phase.unplaced]
  | some (.live _) => simp
  | some (.detaching _ _) => simp
  | some (.held _ _) => simp
  | some .failed => simp
  | some (.erasing h _) => cases h <;> simp
  | some .erased => simp

theorem memSum_le_taken (c : Cfg) (f : Nat → Option Slot) : memSum c f ≤ taken c f :=
  S_le (fun j _ => by
    by_cases hp : isPinned c j <;> simp [hp, mem_le_place])

/-- The invariant survives replacing slot `k` by `x` when the permits are
accounted for, the on-demand `Memory`s and the unplaced attaches do not grow,
an on-demand-only slot lands only on an on-demand session, and a retry that
holds its permit still finds its `HeldElsewhere`. -/
theorem inv_upd {c : Cfg} (hc : WF c) {s s' : St} (hI : Inv c s) {k : Nat} {x : Option Slot}
    (hk : k ∈ c.ids) (hslots : s'.slots = upd s.slots k x) (hretry : s'.retry = s.retry)
    (hround : s'.round = s.round) (hfree : s'.free + holds x = s.free + holds (s.slots k))
    (hm : memSum c s'.slots ≤ c.places) (hu : unplSum c s'.slots ≤ c.cap)
    (hod : odOnly x = true → isPinned c k = false)
    (hhe : s.retry = some (k, true) → isHeld x = true) : Inv c s' := by
  obtain ⟨h1, -, -, -⟩ := sums_upd hc hk s.slots x
  have hp0 := hI.permits
  unfold holdSum at hp0
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, hm, hu⟩
  · intro j hj
    have : j ≠ k := fun e => hj (e ▸ hk)
    rw [hslots, upd_ne _ _ this]
    exact hI.out j hj
  · intro j hj
    rw [hslots] at hj
    by_cases e : j = k
    · subst e
      rw [upd_same] at hj
      exact hod hj
    · rw [upd_ne _ _ e] at hj
      exact hI.od j hj
  · rw [hretry]; exact hI.retryPin
  · rw [hround]; exact hI.roundPin
  · intro j hj
    rw [hretry] at hj
    rw [hslots]
    by_cases e : j = k
    · subst e
      rw [upd_same]
      exact hhe hj
    · rw [upd_ne _ _ e]
      exact hI.retryHeld j hj
  · unfold holdSum
    rw [hslots, hretry]
    omega

/-- The common case of `inv_upd`'s `memSum` obligation: the slot's own
weight does not grow. -/
theorem memSum_upd_le {c : Cfg} (hc : WF c) {f : Nat → Option Slot} {k : Nat} {x : Option Slot}
    (hk : k ∈ c.ids) (hm : mem x ≤ mem (f k)) : memSum c (upd f k x) ≤ memSum c f := by
  obtain ⟨-, h2, -, -⟩ := sums_upd hc hk f x
  by_cases hp : isPinned c k <;> simp [hp] at h2 <;> omega

theorem unplSum_upd_le {c : Cfg} (hc : WF c) {f : Nat → Option Slot} {k : Nat} {x : Option Slot}
    (hk : k ∈ c.ids) (hu : unpl x ≤ unpl (f k)) : unplSum c (upd f k x) ≤ unplSum c f := by
  obtain ⟨-, -, h3, -⟩ := sums_upd hc hk f x
  omega

/-- The invariant survives a change of anything the invariant does not read. -/
theorem inv_congr {c : Cfg} {s s' : St} (hI : Inv c s) (hs : s'.slots = s.slots)
    (hf : s'.free = s.free) (hr : s'.retry = s.retry) (hro : s'.round = s.round) : Inv c s' := by
  have hp0 := hI.permits
  unfold holdSum at hp0
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · rw [hs]; exact hI.out
  · rw [hs]; exact hI.od
  · rw [hr]; exact hI.retryPin
  · rw [hro]; exact hI.roundPin
  · rw [hs, hr]; exact hI.retryHeld
  · unfold holdSum; rw [hs, hr, hf]; exact hp0
  · rw [hs]; exact hI.mems
  · rw [hs]; exact hI.unplaced

/-- The invariant survives rewriting every slot by `F`, when `F` keeps
absent slots absent, makes no on-demand-only slot, changes no permit holder,
and lets neither the `Memory`s nor the unplaced attaches grow, nor a
`HeldElsewhere` go. -/
theorem inv_map {c : Cfg} {s s' : St} (hI : Inv c s) (F : Nat → Option Slot → Option Slot)
    (hs : ∀ j, s'.slots j = F j (s.slots j)) (hr : s'.retry = s.retry) (hro : s'.round = s.round)
    (hf : s'.free = s.free)
    (hnone : ∀ j, F j none = none)
    (hod : ∀ j x, odOnly (F j x) = true → odOnly x = true)
    (hh : ∀ j x, holds (F j x) = holds x) (hm : ∀ j x, mem (F j x) ≤ mem x)
    (hu : ∀ j x, unpl (F j x) = unpl x) (hhe : ∀ j x, isHeld x = true → isHeld (F j x) = true) :
    Inv c s' := by
  have hp0 := hI.permits
  unfold holdSum at hp0
  have hfun : s'.slots = fun j => F j (s.slots j) := funext hs
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro j hj; rw [hs, hI.out j hj, hnone]
  · intro j hj; rw [hs] at hj; exact hI.od j (hod j _ hj)
  · rw [hr]; exact hI.retryPin
  · rw [hro]; exact hI.roundPin
  · intro j hj; rw [hr] at hj; rw [hs]; exact hhe j _ (hI.retryHeld j hj)
  · unfold holdSum
    rw [hfun, hr, hf, S_congr (fun j _ => hh j (s.slots j))]
    exact hp0
  · rw [hfun]
    exact Nat.le_trans (S_le (fun j _ => by
      by_cases hp : isPinned c j <;> simp [hp, hm])) hI.mems
  · rw [hfun]
    unfold unplSum
    rw [S_congr (fun j _ => hu j (s.slots j))]
    exact hI.unplaced

/-! ### Each event keeps the invariant -/

section Preserve

variable {c : Cfg} {s s' : St}

/-- `inv_upd` when neither the `Memory`s nor the unplaced attaches grow at the slot. -/
theorem inv_upd_le (hc : WF c) (hI : Inv c s) {k : Nat} {x : Option Slot} (hk : k ∈ c.ids)
    (hslots : s'.slots = upd s.slots k x) (hretry : s'.retry = s.retry)
    (hround : s'.round = s.round) (hfree : s'.free + holds x = s.free + holds (s.slots k))
    (hm : mem x ≤ mem (s.slots k)) (hu : unpl x ≤ unpl (s.slots k))
    (hod : odOnly x = true → isPinned c k = false)
    (hhe : s.retry = some (k, true) → isHeld x = true) : Inv c s' :=
  inv_upd hc hI hk hslots hretry hround hfree
    (by rw [hslots]; exact Nat.le_trans (memSum_upd_le hc hk hm) hI.mems)
    (by rw [hslots]; exact Nat.le_trans (unplSum_upd_le hc hk hu) hI.unplaced) hod hhe

/-- No retry holds its permit over a slot that is not `HeldElsewhere`. -/
theorem no_retry_of_not_held (hI : Inv c s) {k : Nat} (hk : isHeld (s.slots k) = false) :
    s.retry = some (k, true) → False := by
  intro hr
  have := hI.retryHeld k hr
  rw [hk] at this
  cases this

theorem inv_start (hc : WF c) (hI : Inv c s) {i : Nat} {cr : Bool} (h : evStart c s i cr = some s') : Inv c s' := by
  unfold evStart at h
  split at h
  · rename_i hg
    obtain ⟨hi, hp, -, hn, -, hu, -⟩ := hg
    cases h
    obtain ⟨-, -, h3, -⟩ := sums_upd hc hi s.slots (some (.attaching cr .queued))
    rw [hn] at h3
    simp [Phase.unplaced] at h3
    refine inv_upd hc hI hi rfl rfl rfl (by rw [hn]; simp [Phase.holds]) ?_ ?_
      (fun _ => hp) (fun hr => absurd hr (no_retry_of_not_held hI (by simp [hn])))
    · exact Nat.le_trans (memSum_upd_le hc hi (by rw [hn]; simp [Phase.mem])) hI.mems
    · simp only; omega
  · cases h

theorem inv_enter (hc : WF c) (hI : Inv c s) {i : Nat} (h : evEnter s i = some s') : Inv c s' := by
  unfold evEnter at h
  split at h
  · rename_i hh0 heq
    cases h
    exact inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [heq]) (by simp [heq])
      (by simp [heq]) (by simp [odOnly]) (fun hr => absurd hr (no_retry_of_not_held hI (by simp [heq])))
  · cases h

theorem inv_leave (hc : WF c) (hI : Inv c s) {i : Nat} (h : evLeave s i = some s') : Inv c s' := by
  unfold evLeave at h
  split at h
  · rename_i hh0 heq
    split at h
    · cases h
    · cases h
      exact inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [heq]) (by simp [heq])
        (by simp [heq]) (by simp [odOnly]) (fun hr => absurd hr (no_retry_of_not_held hI (by simp [heq])))
  · cases h

theorem inv_write (hI : Inv c s) {i : Nat} (h : evWrite s i = some s') : Inv c s' := by
  unfold evWrite at h
  split at h
  · split at h
    · cases h
    · cases h; exact inv_congr hI rfl rfl rfl rfl
  · cases h

theorem inv_endFlight (hc : WF c) (hI : Inv c s) {i : Nat} {cr : Bool} {p : Phase}
    (heq : s.slots i = some (.attaching cr p)) {back : Nat} (hb : back = p.holds) :
    Inv c (endFlight s i back) :=
  inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [endFlight, heq, hb]) (by simp)
    (by simp) (by simp [odOnly]) (fun hr => absurd hr (no_retry_of_not_held hI (by simp [heq])))

theorem evictable_spec {v : Nat} {hv : H} (h : evictable c s v = some hv) :
    s.slots v = some (.live hv) ∧ isPinned c v = false ∧ hv.inflight = 0 ∧
      hv.used + c.minIdle ≤ s.now := by
  unfold evictable at h
  split at h
  · rename_i h0 heq
    split at h
    · rename_i hg
      cases h
      exact ⟨heq, hg.1, hg.2.1, hg.2.2⟩
    · cases h
  · cases h

theorem inv_reserve (hc : WF c) (hI : Inv c s) {i : Nat} {cr : Bool} {victim : Option Nat}
    (heq : s.slots i = some (.attaching cr .probing)) (h : reserve c s i cr victim = some s') :
    Inv c s' := by
  have hi := mem_of_some hI heq
  have hp := hI.attPin heq
  have hnr := no_retry_of_not_held hI (k := i) (by simp [heq])
  unfold reserve at h
  split at h
  · rename_i ht
    cases h
    obtain ⟨-, h2, -, -⟩ := sums_upd hc hi s.slots (some (.attaching cr .acquiring))
    rw [heq] at h2
    simp [hp, Phase.mem] at h2
    have := memSum_le_taken c s.slots
    refine inv_upd hc hI hi rfl rfl rfl (by simp [heq, Phase.holds]) (by simp only; omega)
      (Nat.le_trans (unplSum_upd_le hc hi (by simp [heq, Phase.unplaced])) hI.unplaced)
      (fun _ => hp) (fun hr => absurd hr hnr)
  · split at h
    · cases h
      exact inv_endFlight hc hI heq rfl
    · split at h
      · rename_i v _ hv hev
        cases h
        obtain ⟨hsv, hpv, -, -⟩ := evictable_spec hev
        have hvi : i ≠ v := fun e => by rw [e, hsv] at heq; cases heq
        let s1 : St := { s with slots := upd s.slots v (some (.detaching hv (some i))) }
        have h1 : Inv c s1 :=
          inv_upd_le hc hI (mem_of_some hI hsv) rfl rfl rfl (by simp [hsv, s1]) (by simp [hsv])
            (by simp [hsv]) (fun _ => hpv)
            (fun hr => absurd hr (no_retry_of_not_held hI (by simp [hsv])))
        have hs1i : s1.slots i = s.slots i := upd_ne _ _ hvi
        exact inv_upd_le hc h1 hi rfl rfl rfl
          (by rw [hs1i, heq]; simp [Phase.holds, s1]) (by rw [hs1i, heq]; simp [Phase.mem])
          (by rw [hs1i, heq]; simp [Phase.unplaced]) (fun _ => hp)
          (fun hr => absurd hr (no_retry_of_not_held h1 (by rw [hs1i, heq]; simp)))
      · cases h

theorem inv_fPermit (hc : WF c) (hI : Inv c s) {i : Nat} (h : evFPermit c s i = some s') :
    Inv c s' := by
  unfold evFPermit at h
  split at h
  · rename_i cr heq
    have hi := mem_of_some hI heq
    have hnr := no_retry_of_not_held hI (k := i) (by simp [heq])
    split at h
    · cases h; exact inv_endFlight hc hI heq rfl
    · split at h
      · cases h
      · split at h
        · cases h; exact inv_endFlight hc hI heq rfl
        · cases h
          exact inv_upd_le hc hI hi rfl rfl rfl (by simp [heq, Phase.holds]; omega)
            (by simp [heq, Phase.mem]) (by simp [heq, Phase.unplaced]) (fun _ => hI.attPin heq)
            (fun hr => absurd hr hnr)
  · cases h

theorem inv_fProbe (hc : WF c) (hI : Inv c s) {i : Nat} {err : Bool} {v : Option Nat}
    (h : evFProbe c s i err v = some s') : Inv c s' := by
  unfold evFProbe at h
  split at h
  · rename_i cr heq
    have hend : Inv c (endFlight s i 1) := inv_endFlight hc hI heq rfl
    split at h
    · cases h; exact hend
    · split at h <;> (try split at h) <;>
        first
        | (cases h; exact hend)
        | (cases h; exact inv_congr hend rfl rfl rfl rfl)
        | exact inv_reserve hc hI heq h
        | cases h
  · cases h

theorem inv_fGo (hc : WF c) (hI : Inv c s) {i : Nat} (h : evFGo s i = some s') : Inv c s' := by
  unfold evFGo at h
  split at h
  · rename_i cr v heq
    split at h
    · cases h; exact inv_endFlight hc hI heq rfl
    · cases h
  · rename_i cr heq
    split at h
    · cases h; exact inv_congr (inv_endFlight hc hI heq rfl) rfl rfl rfl rfl
    · cases h
      exact inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [heq, Phase.holds])
        (by simp [heq, Phase.mem]) (by simp [heq, Phase.unplaced]) (fun _ => hI.attPin heq)
        (fun hr => absurd hr (no_retry_of_not_held hI (by simp [heq])))
  · cases h

theorem inv_fTimeout (hc : WF c) (hI : Inv c s) {i : Nat} (h : evFTimeout s i = some s') :
    Inv c s' := by
  unfold evFTimeout at h
  split at h
  · rename_i cr p heq
    split at h
    · cases h; exact inv_congr (inv_endFlight hc hI heq rfl) rfl rfl rfl rfl
    · cases h; exact inv_endFlight hc hI heq rfl
  · cases h

theorem inv_fDrop (hc : WF c) (hI : Inv c s) {i : Nat} (h : evFDrop s i = some s') : Inv c s' := by
  unfold evFDrop at h
  split at h
  · rename_i cr p heq
    cases h; exact inv_congr (inv_endFlight hc hI heq rfl) rfl rfl rfl rfl
  · cases h

theorem inv_fAdmit (hc : WF c) (hI : Inv c s) {i : Nat} {ok : Bool} (h : evFAdmit c s i ok = some s') :
    Inv c s' := by
  unfold evFAdmit at h
  split at h
  · rename_i cr heq
    split at h
    · cases h; exact inv_endFlight hc hI heq rfl
    · split at h
      · cases h
        exact inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [heq, Phase.holds])
          (by simp [heq, Phase.mem]) (by simp [heq, Phase.unplaced]) (by simp [odOnly])
          (fun hr => absurd hr (no_retry_of_not_held hI (by simp [heq])))
      · cases h; exact inv_congr (inv_endFlight hc hI heq rfl) rfl rfl rfl rfl
  · cases h

theorem inv_abandRun (hI : Inv c s) {i : Nat} (h : evAbandRun s i = some s') : Inv c s' := by
  unfold evAbandRun at h
  split at h
  · cases h; exact inv_congr hI rfl rfl rfl rfl
  · cases h

theorem inv_rRound (hI : Inv c s) (h : evRRound c s = some s') : Inv c s' := by
  unfold evRRound at h
  split at h
  · cases h
    exact { hI with roundPin := fun j hj => (List.mem_filter.1 hj).1 }
  · cases h

theorem inv_rStart (hI : Inv c s) (h : evRStart s = some s') : Inv c s' := by
  unfold evRStart at h
  split at h
  · rename_i j rest hr hro
    cases h
    have hp0 := hI.permits
    unfold holdSum at hp0
    rw [hr] at hp0
    refine ⟨hI.out, hI.od, ?_, ?_, ?_, ?_, hI.mems, hI.unplaced⟩
    · intro j' b e
      cases e
      exact hI.roundPin j (by rw [hro]; exact List.mem_cons_self)
    · intro j' hj'
      exact hI.roundPin j' (by rw [hro]; exact List.mem_cons_of_mem _ hj')
    · intro j' e
      cases e
    · unfold holdSum
      simp only [retryHolds] at hp0 ⊢
      omega
  · cases h

/-- The retry ends without its permit, or gives it back. -/
theorem inv_retry_none (hI : Inv c s) {back : Nat} (hb : back = retryHolds s.retry) :
    Inv c { s with retry := none, free := s.free + back } := by
  have hp0 := hI.permits
  unfold holdSum at hp0
  refine ⟨hI.out, hI.od, ?_, hI.roundPin, ?_, ?_, hI.mems, hI.unplaced⟩
  · intro j b' e; cases e
  · intro j e; cases e
  · unfold holdSum
    simp only [retryHolds]
    rw [hb] at *
    omega

theorem memSum_upd_pinned {f : Nat → Option Slot} {k : Nat} {x : Option Slot}
    (hc : WF c) (hk : k ∈ c.ids) (hp : isPinned c k = true) : memSum c (upd f k x) = memSum c f := by
  obtain ⟨-, h2, -, -⟩ := sums_upd hc hk f x
  simp [hp] at h2
  exact h2

theorem inv_rPermit (hc : WF c) (hI : Inv c s) (h : evRPermit c s = some s') : Inv c s' := by
  unfold evRPermit at h
  split at h
  · rename_i i hr
    have hpin := hI.retryPin i false hr
    have hrn : Inv c { s with retry := none, free := s.free + 0 } :=
      inv_retry_none hI (by rw [hr]; rfl)
    split at h
    · cases h; exact inv_congr hrn rfl (by simp) rfl rfl
    · split at h
      · cases h
      · split at h
        · rename_i r q heq
          have hi := mem_of_some hI heq
          -- the slot keeps `HeldElsewhere`; the retry takes its permit
          have go : ∀ (s1 : St), s1.slots = upd s.slots i (some (.held r none)) →
              s1.retry = some (i, true) → s1.free = s.free - 1 → s1.round = s.round →
              Inv c s1 := by
            intro s1 hs1 hr1 hf1 hro1
            have hp0 := hI.permits
            unfold holdSum at hp0
            rw [hr] at hp0
            obtain ⟨h1, -, -, -⟩ := sums_upd hc hi s.slots (some (.held r none))
            rw [heq] at h1
            simp at h1
            refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
            · intro j hj
              have : j ≠ i := fun e => hj (e ▸ hi)
              rw [hs1, upd_ne _ _ this]; exact hI.out j hj
            · intro j hj
              rw [hs1] at hj
              by_cases e : j = i
              · subst e; rw [upd_same] at hj; simp [odOnly] at hj
              · rw [upd_ne _ _ e] at hj; exact hI.od j hj
            · intro j b e; rw [hr1] at e; cases e; exact hpin
            · rw [hro1]; exact hI.roundPin
            · intro j e; rw [hr1] at e; cases e; rw [hs1]; simp
            · unfold holdSum
              rw [hs1, hr1, hf1]
              simp only [retryHolds] at hp0 ⊢
              omega
            · rw [hs1, memSum_upd_pinned hc hi (by simp [isPinned, hpin])]; exact hI.mems
            · rw [hs1]
              exact Nat.le_trans (unplSum_upd_le hc hi (by simp [heq])) hI.unplaced
          split at h
          · split at h
            · cases h
              exact inv_upd_le hc hrn hi rfl rfl rfl (by simp [heq]) (by simp [heq]) (by simp [heq])
                (by simp [odOnly]) (fun e => by cases e)
            · cases h; exact go _ rfl rfl rfl rfl
          · cases h; exact go _ rfl rfl rfl rfl
        · cases h; exact inv_congr hrn rfl (by simp) rfl rfl
  · cases h

/-- `replace_held` over a pinned `HeldElsewhere` with a slot of no weight. -/
theorem inv_replaceHeld (hc : WF c) (hI : Inv c s) {i : Nat} (hpin : i ∈ c.pinned) (x : Slot)
    (hx : holds (some x) = 0 ∧ unpl (some x) = 0 ∧ mem (some x) = 0 ∧ odOnly (some x) = false)
    (hr : s.retry = none) : Inv c { s with slots := replaceHeld s.slots i x } := by
  unfold replaceHeld
  split
  · rename_i r q heq
    exact inv_upd_le hc hI (hc.pinned i hpin) rfl rfl rfl (by simp [heq, hx.1])
      (by simp [hx.2.2.1]) (by simp [hx.2.1]) (by simp [hx.2.2.2]) (fun e => by rw [hr] at e; cases e)
  · exact inv_congr hI rfl rfl rfl rfl

theorem inv_rResult (hc : WF c) (hI : Inv c s) {o : ROut} (h : evRResult c s o = some s') :
    Inv c s' := by
  unfold evRResult at h
  split at h
  · rename_i i hr
    have hpin := hI.retryPin i true hr
    have hi := hc.pinned i hpin
    have hb : Inv c (retryDone s) := inv_retry_none hI (by rw [hr]; rfl)
    have hbr : (retryDone s).retry = none := rfl
    have hw (x : Slot) (hx : holds (some x) = 0 ∧ unpl (some x) = 0 ∧ mem (some x) = 0 ∧
        odOnly (some x) = false) : Inv c { retryDone s with slots := replaceHeld s.slots i x } :=
      inv_replaceHeld hc hb hpin x hx hbr
    split at h
    · split at h
      · split at h
        · rename_i r _ hh
          cases h
          obtain ⟨r0, q0, heq⟩ : ∃ r0 q0, s.slots i = some (.held r0 q0) := by
            revert hh; cases e : s.slots i with
            | none => simp
            | some x => cases x <;> simp
          refine inv_upd hc hb hi rfl rfl rfl (by simp [retryDone, heq]) ?_ ?_ (by simp [odOnly])
            (fun e => by cases e)
          · show memSum c (upd s.slots i _) ≤ c.places
            rw [memSum_upd_pinned hc hi (by simp [isPinned, hpin])]; exact hI.mems
          · exact Nat.le_trans (unplSum_upd_le hc hi (by simp [heq])) hI.unplaced
        · cases h; exact inv_congr hb rfl rfl rfl rfl
      · cases h
    all_goals (try split at h) <;> cases h <;> first
      | exact hw _ (by simp [odOnly])
      | exact hb
      | exact inv_congr hb rfl rfl rfl rfl
      | exact inv_congr (hw _ (by simp [odOnly])) rfl rfl rfl rfl
  · cases h

theorem mapLD_w (x : Option Slot) (f : H → H) :
    holds (mapLD x f) = holds x ∧ mem (mapLD x f) = mem x ∧ unpl (mapLD x f) = unpl x ∧
      odOnly (mapLD x f) = odOnly x ∧ isHeld (mapLD x f) = isHeld x := by
  unfold mapLD
  split
  · simp [odOnly]
  · rename_i h w
    cases w <;> simp [odOnly]
  · simp

theorem ldHandle_spec {x : Option Slot} {h : H} (hl : ldHandle x = some h) :
    isHeld x = false ∧ ∃ y, x = some y := by
  unfold ldHandle at hl
  split at hl
  · exact ⟨rfl, _, rfl⟩
  · exact ⟨rfl, _, rfl⟩
  · cases hl

/-- An event that changes only the handle of a live or detaching slot. -/
theorem inv_mapLD (hc : WF c) (hI : Inv c s) {i : Nat} {h0 : H}
    (hl : ldHandle (s.slots i) = some h0) (f : H → H)
    (hs : s'.slots = upd s.slots i (mapLD (s.slots i) f)) (hr : s'.retry = s.retry)
    (hro : s'.round = s.round) (hf : s'.free = s.free) : Inv c s' := by
  obtain ⟨hnh, y, hy⟩ := ldHandle_spec hl
  obtain ⟨w1, w2, w3, w4, -⟩ := mapLD_w (s.slots i) f
  exact inv_upd_le hc hI (mem_of_some hI hy) hs hr hro (by rw [hf, w1]) (Nat.le_of_eq w2)
    (Nat.le_of_eq w3)
    (by rw [w4]; exact hI.od i) (fun e => absurd e (no_retry_of_not_held hI hnh))

theorem inv_watchPark (hc : WF c) (hI : Inv c s) {i : Nat} (h : evWatchPark s i = some s') :
    Inv c s' := by
  unfold evWatchPark at h
  split at h
  · rename_i h0 hl
    split at h
    · cases h; exact inv_mapLD hc hI hl _ rfl rfl rfl rfl
    · cases h
  · cases h

theorem inv_watchFire (hc : WF c) (hI : Inv c s) {i : Nat} (h : evWatchFire s i = some s') :
    Inv c s' := by
  unfold evWatchFire at h
  split at h
  · rename_i h0 hl
    split at h
    · cases h; exact inv_mapLD hc hI hl _ rfl rfl rfl rfl
    · cases h
  · cases h

theorem inv_beat (hc : WF c) (hI : Inv c s) {i : Nat} (h : evBeat s i = some s') : Inv c s' := by
  unfold evBeat at h
  split at h
  · rename_i h0 hl
    split at h
    · cases h
    · split at h
      · cases h; exact hI
      · cases h; exact inv_mapLD hc hI hl _ rfl rfl rfl rfl
  · cases h

theorem inv_stale (hc : WF c) (hI : Inv c s) {i : Nat} (h : evStale s i = some s') : Inv c s' := by
  unfold evStale at h
  split at h
  · rename_i h0 hl
    split at h
    · cases h; exact inv_mapLD hc hI hl _ rfl rfl rfl rfl
    · cases h
  · cases h

theorem inv_lostRun (hc : WF c) (hI : Inv c s) {i : Nat} (h : evLostRun s i = some s') :
    Inv c s' := by
  unfold evLostRun at h
  split at h
  · dsimp only at h
    split at h
    · rename_i lh heq
      cases h
      exact inv_upd_le hc hI (mem_of_some hI heq) rfl rfl rfl (by simp [heq]) (by simp [heq])
        (by simp [heq]) (by simp [odOnly]) (fun e => absurd e (no_retry_of_not_held hI (by simp [heq])))
    · cases h; exact inv_congr hI rfl rfl rfl rfl
  · cases h

theorem sweepSlot_w (n j : Nat) (x : Option Slot) :
    sweepSlot c n j none = none ∧ holds (sweepSlot c n j x) = holds x ∧
      mem (sweepSlot c n j x) ≤ mem x ∧ unpl (sweepSlot c n j x) = unpl x ∧
      (odOnly (sweepSlot c n j x) = true → odOnly x = true) ∧
      (isHeld x = true → isHeld (sweepSlot c n j x) = true) := by
  refine ⟨rfl, ?_⟩
  unfold sweepSlot
  split
  · split <;> simp [odOnly]
  · simp

theorem inv_sweep (hI : Inv c s) (h : evSweep c s = some s') : Inv c s' := by
  unfold evSweep at h
  split at h
  · cases h
    exact inv_map hI (sweepSlot c s.now) (fun _ => rfl) rfl rfl rfl
      (fun j => (sweepSlot_w s.now j none).1) (fun j x => (sweepSlot_w s.now j x).2.2.2.2.1)
      (fun j x => (sweepSlot_w s.now j x).2.1) (fun j x => (sweepSlot_w s.now j x).2.2.1)
      (fun j x => (sweepSlot_w s.now j x).2.2.2.1) (fun j x => (sweepSlot_w s.now j x).2.2.2.2.2)
  · cases h

/-- The evicting flight is told its victim is gone; there is room for it,
since the victim's `Memory` is gone too. -/
theorem inv_wake (hc : WF c) {s1 s2 : St} (hI1 : Inv c s1) {i : Nat} {w : Option Nat}
    (hs2 : s2.slots = wake s1.slots i w) (hr : s2.retry = s1.retry) (hro : s2.round = s1.round)
    (hf : s2.free = s1.free) (hroom : w ≠ none → memSum c s1.slots + 1 ≤ c.places) : Inv c s2 := by
  cases w with
  | none => exact inv_congr hI1 hs2 hf hr hro
  | some f =>
    have room := hroom (by simp)
    simp only [wake] at hs2
    split at hs2
    · rename_i cr v heq
      split at hs2
      · have hfi := mem_of_some hI1 heq
        have hp := hI1.attPin heq
        obtain ⟨-, h2, -, -⟩ := sums_upd hc hfi s1.slots (some (.attaching cr .ready))
        rw [heq] at h2
        simp [hp, Phase.mem] at h2
        exact inv_upd hc hI1 hfi hs2 hr hro (by rw [hf, heq]; simp [Phase.holds])
          (by rw [hs2]; omega)
          (by rw [hs2]; exact Nat.le_trans (unplSum_upd_le hc hfi (by simp [heq, Phase.unplaced]))
                hI1.unplaced)
          (fun _ => hp) (fun e => absurd e (no_retry_of_not_held hI1 (by simp [heq])))
      · exact inv_congr hI1 hs2 hf hr hro
    · exact inv_congr hI1 hs2 hf hr hro

theorem inv_detachRun (hc : WF c) (hI : Inv c s) {i : Nat} (h : evDetachRun c s i = some s') :
    Inv c s' := by
  unfold evDetachRun at h
  split at h
  · rename_i dh w heq
    have hi := mem_of_some hI heq
    have hnr := no_retry_of_not_held hI (k := i) (by simp [heq])
    have hroom : ∀ (y : Option Slot), mem y = 0 → w ≠ none →
        memSum c (upd s.slots i y) + 1 ≤ c.places := by
      intro y hy hw
      have hod : isPinned c i = false := hI.od i (by
        rw [heq]; cases w with
        | none => exact absurd rfl hw
        | some _ => rfl)
      obtain ⟨-, h2, -, -⟩ := sums_upd hc hi s.slots y
      rw [heq] at h2
      simp [hod, hy] at h2
      have := hI.mems
      omega
    dsimp only at h
    split at h
    · cases h
      let s1 : St := { s with slots := upd s.slots i (some (.held (s.now + c.pr) (some (dh.hid, s.now)))) }
      have h1 : Inv c s1 :=
        inv_upd_le hc hI hi rfl rfl rfl (by simp [heq, s1]) (by simp [heq]) (by simp [heq])
          (by simp [odOnly]) (fun e => absurd e hnr)
      exact inv_wake hc h1 rfl rfl rfl rfl (hroom _ (by simp))
    · cases h
      let s1 : St := { s with slots := upd s.slots i none }
      have h1 : Inv c s1 :=
        inv_upd_le hc hI hi rfl rfl rfl (by simp [heq, s1]) (by simp [heq]) (by simp [heq])
          (by simp [odOnly]) (fun e => absurd e hnr)
      exact inv_wake hc h1 rfl rfl rfl rfl (hroom _ (by simp))
  · cases h

theorem inv_strayDrop (hI : Inv c s) {k : Nat} (h : evStrayDrop s k = some s') : Inv c s' := by
  unfold evStrayDrop at h
  split at h
  · cases h; exact inv_congr hI rfl rfl rfl rfl
  · cases h

theorem inv_rowLapse (hI : Inv c s) {i : Nat} (h : evRowLapse s i = some s') : Inv c s' := by
  unfold evRowLapse at h
  split at h <;> cases h <;> exact inv_congr hI rfl rfl rfl rfl

theorem inv_otherTake (hI : Inv c s) {i : Nat} (h : evOtherTake s i = some s') : Inv c s' := by
  unfold evOtherTake at h
  split at h <;> cases h <;> exact inv_congr hI rfl rfl rfl rfl

theorem inv_eClaim (hc : WF c) (hI : Inv c s) {i : Nat} (h : evEClaim c s i = some s') :
    Inv c s' := by
  unfold evEClaim at h
  split at h
  · rename_i hg
    obtain ⟨hi, hfree, -⟩ := hg
    have hno : s.retry = some (i, true) → False := by
      intro hr
      have := hI.permits
      unfold holdSum at this
      rw [hr] at this
      simp only [retryHolds] at this
      omega
    split at h <;> (try dsimp only at h) <;> cases h <;>
      exact inv_upd_le hc hI hi rfl rfl rfl (by simp [*]) (by simp [*]) (by simp [*])
        (by simp [odOnly]) (fun hr => absurd hr hno)
  · cases h

theorem prior_w (p : Prior) :
    holds p.slot = 0 ∧ mem p.slot = 0 ∧ unpl p.slot = 0 ∧ odOnly p.slot = false := by
  cases p <;> simp [Prior.slot, odOnly]

theorem inv_eEnd (hc : WF c) (hI : Inv c s) {i : Nat} {err : Bool} (h : evEEnd c s i err = some s') :
    Inv c s' := by
  unfold evEEnd at h
  have hset : ∀ (s0 : St) (x : Option Slot), Inv c s0 → s0.slots i = s.slots i →
      (∃ eh q, s.slots i = some (.erasing eh q)) → holds x = 0 → mem x = 0 → unpl x = 0 →
      odOnly x = false → Inv c { s0 with slots := upd s0.slots i x } := by
    intro s0 x hI0 hs0 ⟨eh, q, heq⟩ h1 h2 h3 h4
    have hi := mem_of_some hI heq
    exact inv_upd_le hc hI0 hi rfl rfl rfl (by simp [hs0, heq, h1]) (by simp [h2])
      (by simp [h3]) (by simp [h4])
      (fun e => absurd e (no_retry_of_not_held hI0 (by rw [hs0, heq]; simp)))
  have hmark : ∀ (s0 : St), Inv c s0 → s0.slots i = s.slots i →
      (∃ eh q, s.slots i = some (.erasing eh q)) → Inv c { s0 with slots := markErased c s0.slots i } := by
    intro s0 hI0 hs0 hex
    unfold markErased
    split
    · exact hset s0 _ hI0 hs0 hex rfl rfl rfl rfl
    · exact hset s0 _ hI0 hs0 hex rfl rfl rfl rfl
  split at h
  · rename_i eh q heq
    have hex : ∃ eh q, s.slots i = some (.erasing eh q) := ⟨_, _, heq⟩
    have haf : ∀ (s0 : St), Inv c s0 → s0.slots i = s.slots i → Inv c (afterFailed c s0 i eh) := by
      intro s0 hI0 hs0
      unfold afterFailed
      split
      · exact inv_congr (hset s0 _ hI0 hs0 hex rfl rfl rfl rfl) rfl rfl rfl rfl
      · exact inv_congr (hset s0 _ hI0 hs0 hex rfl rfl rfl rfl) rfl rfl rfl rfl
    split at h
    · cases h; exact haf s hI rfl
    · cases h; exact hmark s hI rfl hex
    · split at h
      · cases h; exact haf _ (inv_congr hI rfl rfl rfl rfl) rfl
      · cases h; exact inv_congr (hmark s hI rfl hex) rfl rfl rfl rfl
  · rename_i q heq
    have hex : ∃ eh q, s.slots i = some (.erasing eh q) := ⟨_, _, heq⟩
    obtain ⟨p1, p2, p3, p4⟩ := prior_w q
    split at h
    · cases h; exact hset s _ hI rfl hex p1 p2 p3 p4
    · cases h; exact hset s _ hI rfl hex p1 p2 p3 p4
    · cases h; exact hmark s hI rfl hex
    · split at h
      · cases h; exact hset s _ hI rfl hex p1 p2 p3 p4
      · cases h; exact inv_congr (hmark s hI rfl hex) rfl rfl rfl rfl
  · cases h

/-- `close_set`'s rewrite of one slot: a live session leaves for the close. -/
def closeSlot (x : Option Slot) : Option Slot :=
  match x with
  | some (.live _) => none
  | x => x

theorem inv_closeSet (hI : Inv c s) (h : evCloseSet c s = some s') : Inv c s' := by
  unfold evCloseSet at h
  split at h
  · cases h
    refine inv_map hI (fun _ x => closeSlot x) (fun j => by unfold closeSlot; rfl) rfl rfl rfl
      (fun _ => rfl) ?_ ?_ ?_ ?_ ?_ <;> intro j x <;> unfold closeSlot <;> split <;> simp [odOnly]
  · cases h

/-- Every step keeps the invariant. -/
theorem inv_step (hc : WF c) (hI : Inv c s) {e : Ev} (h : step c s e = some s') : Inv c s' := by
  cases e with
  | tick d => cases h; exact inv_congr hI rfl rfl rfl rfl
  | start i cr => exact inv_start hc hI h
  | enter i => exact inv_enter hc hI h
  | leave i => exact inv_leave hc hI h
  | write i => exact inv_write hI h
  | fPermit i => exact inv_fPermit hc hI h
  | fProbe i err v => exact inv_fProbe hc hI h
  | fGo i => exact inv_fGo hc hI h
  | fTimeout i => exact inv_fTimeout hc hI h
  | fDrop i => exact inv_fDrop hc hI h
  | fAdmit i ok => exact inv_fAdmit hc hI h
  | abandRun i => exact inv_abandRun hI h
  | rRound => exact inv_rRound hI h
  | rStart => exact inv_rStart hI h
  | rPermit => exact inv_rPermit hc hI h
  | rResult o => exact inv_rResult hc hI h
  | sweep => exact inv_sweep hI h
  | watchPark i => exact inv_watchPark hc hI h
  | watchFire i => exact inv_watchFire hc hI h
  | lostRun i => exact inv_lostRun hc hI h
  | detachRun i => exact inv_detachRun hc hI h
  | strayDrop k => exact inv_strayDrop hI h
  | rowLapse i => exact inv_rowLapse hI h
  | otherTake i => exact inv_otherTake hI h
  | beat i => exact inv_beat hc hI h
  | stale i => exact inv_stale hc hI h
  | eClaim i => exact inv_eClaim hc hI h
  | eEnd i err => exact inv_eEnd hc hI h
  | negEvict i =>
    change evNegEvict s i = some s' at h
    unfold evNegEvict at h
    split at h
    · cases h; exact inv_congr hI rfl rfl rfl rfl
    · cases h
  | close => cases h; exact inv_congr hI rfl rfl rfl rfl
  | closeSet => exact inv_closeSet hI h

end Preserve

/-! ## Reachable states -/

/-- A registry as `serve` starts it: every permit free, no task running,
nothing attaching or being taken down; a pinned session is live (its
startup election won), held elsewhere, failed or erased; an on-demand id has
no slot. -/
structure Init (c : Cfg) (s : St) : Prop where
  free : s.free = c.permits
  retry : s.retry = none
  round : s.round = []
  slots : ∀ j, s.slots j = none ∨
    (j ∈ c.pinned ∧ ((∃ h, s.slots j = some (.live h)) ∨ (∃ r, s.slots j = some (.held r none)) ∨
      s.slots j = some .failed ∨ s.slots j = some .erased))

/-- The states reachable from a start. -/
inductive Reach (c : Cfg) : St → Prop
  | init {s : St} : Init c s → Reach c s
  | step {s s' : St} (e : Ev) : Reach c s → step c s e = some s' → Reach c s'

theorem inv_init {c : Cfg} (hc : WF c) {s : St} (h : Init c s) : Inv c s := by
  have hpin : ∀ j, s.slots j ≠ none → isPinned c j = true ∧ j ∈ c.ids := by
    intro j hj
    rcases h.slots j with e | ⟨hp, -⟩
    · exact absurd e hj
    · exact ⟨by simp [isPinned, hp], hc.pinned j hp⟩
  have hw : ∀ j, holds (s.slots j) = 0 ∧ unpl (s.slots j) = 0 ∧ odOnly (s.slots j) = false := by
    intro j
    rcases h.slots j with e | ⟨-, ⟨_, e⟩ | ⟨_, e⟩ | e | e⟩ <;> rw [e] <;> simp [odOnly]
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro j hj
    cases e : s.slots j with
    | none => rfl
    | some x => exact absurd (hpin j (by simp [e])).2 hj
  · intro j hj
    rw [(hw j).2.2] at hj
    cases hj
  · intro j b e; rw [h.retry] at e; cases e
  · intro j hj; rw [h.round] at hj; cases hj
  · intro j e; rw [h.retry] at e; cases e
  · unfold holdSum
    rw [h.retry, h.free, S_congr (g := fun _ => 0) (fun j _ => (hw j).1)]
    have : ∀ l : List Nat, S (fun _ => 0) l = 0 := by
      intro l; induction l with
      | nil => rfl
      | cons _ _ ih => simp [S, ih]
    simp [this, retryHolds]
  · unfold memSum
    have : S (fun j => if isPinned c j = true then 0 else mem (s.slots j)) c.ids = S (fun _ => 0) c.ids :=
      S_congr (fun j _ => by
        by_cases hn : s.slots j = none
        · simp [hn]
        · simp [(hpin j hn).1])
    rw [this]
    have : ∀ l : List Nat, S (fun _ => 0) l = 0 := by
      intro l; induction l with
      | nil => rfl
      | cons _ _ ih => simp [S, ih]
    rw [this]; exact Nat.zero_le _
  · unfold unplSum
    rw [S_congr (g := fun _ => 0) (fun j _ => (hw j).2.1)]
    have : ∀ l : List Nat, S (fun _ => 0) l = 0 := by
      intro l; induction l with
      | nil => rfl
      | cons _ _ ih => simp [S, ih]
    rw [this]; exact Nat.zero_le _

/-- The invariant holds in every reachable state. -/
theorem reach_inv {c : Cfg} (hc : WF c) {s : St} (h : Reach c s) : Inv c s := by
  induction h with
  | init hi => exact inv_init hc hi
  | step e _ hs ih => exact inv_step hc ih hs

theorem run_reach {c : Cfg} {s : St} (hs : Reach c s) : ∀ {es : List Ev} {s' : St},
    run c s es = some s' → Reach c s'
  | [], s', h => by cases h; exact hs
  | e :: es, s', h => by
    unfold run at h
    split at h
    · rename_i s1 h1
      exact run_reach (Reach.step e hs h1) h
    · cases h

/-! ## Headline results over every interleaving -/

section Headline

variable {c : Cfg} {s : St}

theorem S_eq_zero {g : Nat → Nat} : ∀ {l : List Nat}, S g l = 0 → ∀ j ∈ l, g j = 0
  | [], _, _, hj => absurd hj List.not_mem_nil
  | i :: l, h, j, hj => by
    simp only [S] at h
    rcases List.mem_cons.1 hj with e | e
    · subst e; omega
    · exact S_eq_zero (by omega) j e

def isLive : Option Slot → Nat
  | some (.live _) => 1
  | _ => 0

theorem isLive_le_mem (x : Option Slot) : isLive x ≤ mem x := by
  unfold isLive; split <;> simp

/-- The live on-demand sessions. -/
def liveOD (c : Cfg) (f : Nat → Option Slot) : Nat :=
  S (fun j => if isPinned c j then 0 else isLive (f j)) c.ids

/-- **Caps under concurrent admission** (`reserve_place`, design §3.2 step
3; test `eviction_releases_the_lease_and_a_reattach_mints_the_next_token`):
in every interleaving of requests, attaches, evictions, detaches, erases and
the shutdown, the on-demand `Memory`s (live, being detached, being erased,
or an attach past its eviction wait) never exceed the `max_attached - pinned`
places. An attach that evicts takes its place before the victim's detach
ends, but makes its `Memory` only after it. -/
theorem mems_le_places (hc : WF c) (h : Reach c s) : memSum c s.slots ≤ c.places :=
  (reach_inv hc h).mems

theorem live_on_demand_le_places (hc : WF c) (h : Reach c s) : liveOD c s.slots ≤ c.places :=
  Nat.le_trans (S_le (fun j _ => by
    by_cases hp : isPinned c j <;> simp [hp, isLive_le_mem])) (mems_le_places hc h)

/-- **`attach_concurrency`**: at most `attach_permits` attaches hold a permit
at once, pinned retries and on-demand attaches together. -/
theorem attaches_le_permits (hc : WF c) (h : Reach c s) : holdSum c s ≤ c.permits := by
  have := (reach_inv hc h).permits
  omega

/-- **The unplaced bound** (#32, test
`a_flood_of_absent_ids_stays_bounded`): attaches waiting for their probe
never exceed `unplaced_cap(max_attached)`. -/
theorem unplaced_le_cap (hc : WF c) (h : Reach c s) : unplSum c s.slots ≤ c.cap :=
  (reach_inv hc h).unplaced

/-- Acquires in flight for one session: its on-demand attach, its pinned retry. -/
def building (s : St) (i : Nat) : Nat :=
  (match s.slots i with
   | some (.attaching _ .acquiring) => 1
   | _ => 0) + (if s.retry = some (i, true) then 1 else 0)

/-- **Never two concurrent builds for one session** (single-flight, design
§3.2; test `concurrent_first_requests_attach_once`): at most one
`build_attach` of a session is in flight, whichever path started it. -/
theorem one_build (hc : WF c) (h : Reach c s) (i : Nat) : building s i ≤ 1 := by
  have hI := reach_inv hc h
  unfold building
  by_cases hr : s.retry = some (i, true)
  · have := hI.retryHeld i hr
    simp only [hr, ite_true]
    split
    · rename_i heq; rw [heq] at this; cases this
    · omega
  · simp only [hr, ite_false]
    split <;> omega

/-- An on-demand attach starts only over an absent slot: a request that finds
an attach in flight waits on it (single-flight), and one that finds the
session in any other state is answered from it. -/
theorem start_needs_absent {s' : St} {i : Nat} {cr : Bool} (h : evStart c s i cr = some s') :
    s.slots i = none ∧ isPinned c i = false := by
  unfold evStart at h
  split at h
  · rename_i hg; exact ⟨hg.2.2.2.1, hg.2.1⟩
  · cases h

/-- **Erase excludes concurrent attach** (#32, test
`an_erase_racing_the_pinned_retry_waits_for_it_then_erases_what_it_attached`):
an erase claims its slot only when no attach of any session holds a permit,
so none is between its acquire and its admission. In the model that reads:
every on-demand attach is still waiting for its permit and no pinned retry
is acquiring. In the Rust a flight that has given its permit back but not yet
finished (the timeout path awaits `release_abandoned` in between; see
`evFTimeout`) can still hold its `Attaching` slot at the claim; it never
admits, and an erase of its own session answers 503. What holds of the Rust
is the first sentence: no attach is between its acquire and its admission. -/
theorem erase_claim_quiescent (hc : WF c) (h : Reach c s) {i : Nat} {s' : St}
    (he : evEClaim c s i = some s') :
    (∀ j cr p, s.slots j = some (.attaching cr p) → p = .queued) ∧ ∀ j, s.retry ≠ some (j, true) := by
  have hI := reach_inv hc h
  have hfree : s.free = c.permits := by
    unfold evEClaim at he
    split at he
    · rename_i hg; exact hg.2.1
    · cases he
  have hp := hI.permits
  unfold holdSum at hp
  have h0 : S (fun j => holds (s.slots j)) c.ids = 0 := by omega
  have hr0 : retryHolds s.retry = 0 := by omega
  refine ⟨?_, ?_⟩
  · intro j cr p hj
    have := S_eq_zero h0 j (mem_of_some hI hj)
    rw [hj] at this
    cases p <;> simp_all [Phase.holds]
  · intro j e
    rw [e] at hr0
    cases hr0

/-- And while the slot is `Erasing`, no acquire of the session is in flight:
an attach starts only over an absent slot, and a retry acquires only a
`HeldElsewhere` one. -/
theorem erasing_no_build (hc : WF c) (h : Reach c s) {i : Nat} {eh : Option H} {q : Prior}
    (he : s.slots i = some (.erasing eh q)) : building s i = 0 := by
  have hI := reach_inv hc h
  unfold building
  have : ¬ s.retry = some (i, true) := fun hr => by
    have := hI.retryHeld i hr
    rw [he] at this; cases this
  simp [he, this]

/-- **`admit_if` never refuses** (#32): its failure branch is
unreachable. A retry that holds its permit finds its `HeldElsewhere`
untouched, since an erase needs every permit to claim it; an on-demand flight
admits over its own slot, which nothing else replaces. The Rust's test
`a_retry_that_finds_its_slot_changed_releases_what_it_took` forces the slot
("as nothing in the serve now changes it then"); this is that remark, proved. -/
theorem retry_admits_over_held (hc : WF c) (h : Reach c s) {i : Nat}
    (hr : s.retry = some (i, true)) : isHeld (s.slots i) = true :=
  (reach_inv hc h).retryHeld i hr

/-- An admission by a flight is over its own `Attaching` slot. This follows
from `evFAdmit`'s guard; what makes it say something is that only the
flight's own events change an `Attaching` slot (an erase answers it 503, the
sweep, eviction and `close_set` take only live slots, and `detachRun` only
moves a waiting flight from `evicting` to `ready`), which the model shows
event by event in `inv_step` but does not state as one theorem. -/
theorem fAdmit_own {s' : St} {i : Nat} {ok : Bool} (he : evFAdmit c s i ok = some s') :
    ∃ cr, s.slots i = some (.attaching cr .acquiring) := by
  unfold evFAdmit at he
  split at he
  · rename_i cr heq; exact ⟨cr, heq⟩
  · cases he

end Headline

/-! ## One step at a time -/

section Local

variable {c : Cfg} {s s' : St}

/-! ### Lifecycle: the sweeper, eviction, detach -/

/-- **An idle sweep never detaches a session with a call in flight** (design
§3.4; test `the_idle_sweep_skips_a_session_in_use`): the request's
`InFlight` is taken under the slots' lock the sweep chooses under. -/
theorem sweep_skips_in_flight {j : Nat} {h : H} (hj : s.slots j = some (.live h))
    (hf : h.inflight ≠ 0) (he : evSweep c s = some s') : s'.slots j = some (.live h) := by
  unfold evSweep at he
  split at he
  · cases he
    show sweepSlot c s.now j (s.slots j) = _
    rw [hj]; unfold sweepSlot; simp [hf]
  · cases he

/-- Pinned sessions are never idle-detached (test
`an_idle_on_demand_session_is_detached_and_a_pinned_one_never`). -/
theorem sweep_never_pinned {j : Nat} (hp : isPinned c j = true) (he : evSweep c s = some s') :
    s'.slots j = s.slots j := by
  unfold evSweep at he
  split at he
  · cases he
    show sweepSlot c s.now j (s.slots j) = _
    unfold sweepSlot; split
    · simp [hp]
    · rfl
  · cases he

/-- And every on-demand session idle long enough with nothing in flight is
taken out of service in the same round. -/
theorem sweep_takes_idle {j : Nat} {h : H} (hj : s.slots j = some (.live h))
    (hp : isPinned c j = false) (h0 : h.inflight = 0) (hidle : h.used + c.idle ≤ s.now)
    (he : evSweep c s = some s') : s'.slots j = some (.detaching h none) := by
  unfold evSweep at he
  split at he
  · cases he
    show sweepSlot c s.now j (s.slots j) = _
    rw [hj]; unfold sweepSlot; simp [hp, h0, hidle]
  · cases he

/-- **An eviction takes only an idle on-demand session** (#32;
tests `a_routed_request_holds_its_session_against_eviction`,
`a_session_used_just_now_is_not_evicted`): live, not pinned, no call in
flight, idle at least `min_idle_to_evict`, and it is set `Detaching` with the
attach waiting on it. -/
theorem evict_victim {i v : Nat} {cr : Bool} (hi : s.slots i = some (.attaching cr .probing))
    (ht : ¬ taken c s.slots < c.places) (he : reserve c s i cr (some v) = some s') :
    ∃ hv, s.slots v = some (.live hv) ∧ isPinned c v = false ∧ hv.inflight = 0 ∧
      hv.used + c.minIdle ≤ s.now ∧ s'.slots v = some (.detaching hv (some i)) ∧
      s'.slots i = some (.attaching cr (.evicting v)) := by
  unfold reserve at he
  simp only [ht, ↓reduceIte] at he
  split at he
  · rename_i hv hev
    cases he
    obtain ⟨hsv, hpv, h0, hidle⟩ := evictable_spec hev
    have hvi : v ≠ i := fun e => by rw [e, hi] at hsv; cases hsv
    exact ⟨hv, hsv, hpv, h0, hidle, by simp [upd_ne _ _ hvi], by simp⟩
  · cases he

theorem wake_cases (f : Nat → Option Slot) (i w : Nat) :
    wake f i (some w) = f ∨ ∃ cr, f w = some (.attaching cr (.evicting i)) ∧
      wake f i (some w) = upd f w (some (.attaching cr .ready)) := by
  simp only [wake]
  split
  · rename_i cr v heq
    split
    · rename_i hv
      subst hv
      exact Or.inr ⟨cr, heq, rfl⟩
    · exact Or.inl rfl
  · exact Or.inl rfl

theorem wake_other {f : Nat → Option Slot} {i j : Nat} {w : Option Nat}
    (hj : ∀ cr p, f j ≠ some (.attaching cr p)) : wake f i w j = f j := by
  cases w with
  | none => rfl
  | some w =>
    rcases wake_cases f i w with e | ⟨cr, hfw, e⟩
    · rw [e]
    · rw [e]
      have : j ≠ w := fun e' => by subst e'; exact hj _ _ hfw
      exact upd_ne _ _ this

/-- **A detach releases the lease exactly once** (`run_detach`'s close; test
`closing_the_registry_releases_every_lease_and_keeps_the_tokens`): an
unfenced handle's close releases its lease once, holder-scoped and keeping
the token; a fenced one's never does (the lease is another writer's). -/
theorem detach_release {i : Nat} {h : H} {w : Option Nat}
    (hd : s.slots i = some (.detaching h w)) (he : evDetachRun c s i = some s') :
    (h.fenced = false ∧ s'.rel = h.hid :: s.rel ∧ s'.rows i = relMe (s.rows i)) ∨
      (h.fenced = true ∧ s'.rel = s.rel ∧ s'.rows = s.rows) := by
  unfold evDetachRun at he
  rw [hd] at he
  simp only at he
  cases hf : h.fenced
  · split at he <;> cases he <;> simp [hf]
  · split at he <;> cases he <;> simp

/-- And the detached handle leaves its slot: a pinned session waits in
`HeldElsewhere` for its retry, with the handle as `previous`; an on-demand
one's slot is removed and the handle kept in `previous` (#32). -/
theorem detach_leaves {i : Nat} {h : H} {w : Option Nat}
    (hd : s.slots i = some (.detaching h w)) (he : evDetachRun c s i = some s') :
    (isPinned c i = true ∧ s'.slots i = some (.held (s.now + c.pr) (some (h.hid, s.now)))) ∨
      (isPinned c i = false ∧ s'.slots i = none ∧ s'.prevOD i = some (h.hid, s.now)) := by
  unfold evDetachRun at he
  rw [hd] at he
  simp only at he
  split at he
  · rename_i hp
    cases he
    left
    refine ⟨hp, ?_⟩
    show wake _ i w i = _
    rw [wake_other (by simp)]; simp
  · rename_i hp
    cases he
    right
    refine ⟨by simpa using hp, ?_, by simp⟩
    show wake _ i w i = _
    rw [wake_other (by simp)]; simp

/-- The detach `watch_lease` spawns is by session id
(`spawn_detach(id, LeaseLost)`): when it runs it takes down whatever session
is live in the slot, whether or not it is the handle whose lease was lost,
and whatever is in flight on it. -/
theorem lostRun_any_live {i : Nat} {h : H} (hi : i ∈ s.lost) (hl : s.slots i = some (.live h)) :
    ∃ s', evLostRun s i = some s' ∧ s'.slots i = some (.detaching h none) := by
  unfold evLostRun
  simp only [hi, ↓reduceIte, hl]
  exact ⟨_, rfl, by simp⟩

/-! ### The lease, the fence and `watch_lease` (#293) -/

/-- **No write goes through a fenced handle**: the `Memory` write gate
refuses it. -/
theorem write_refused_fenced {i : Nat} {h : H} (hl : s.slots i = some (.live h))
    (hf : h.fenced = true) : evWrite s i = none := by
  unfold evWrite
  simp [hl, hf]

/-- A write is accepted only through a live, unfenced handle with a request
in flight on it. -/
theorem write_through_live {i : Nat} (he : evWrite s i = some s') :
    ∃ h, s.slots i = some (.live h) ∧ h.fenced = false ∧ h.inflight ≠ 0 ∧
      s'.accepted = h.hid :: s.accepted := by
  unfold evWrite at he
  split at he
  · rename_i h heq
    split at he
    · cases he
    · rename_i hg
      cases he
      refine ⟨h, heq, ?_, ?_, rfl⟩
      · cases hf : h.fenced
        · rfl
        · exact absurd (Or.inr hf) hg
      · exact fun e => hg (Or.inl e)
  · cases he

/-- A beat on the handle's own generation changes nothing (`Lease.beat_own_generation`). -/
theorem beat_own_gen {i : Nat} {h : H} (hl : ldHandle (s.slots i) = some h)
    (hb : h.beating = true) (ho : ownGen (s.rows i) h = true) : evBeat s i = some s := by
  unfold evBeat
  simp [hl, hb, ho]

theorem ldHandle_mapLD {x : Option Slot} {h : H} (hl : ldHandle x = some h) (f : H → H) :
    ldHandle (mapLD x f) = some (f h) := by
  unfold ldHandle at hl
  split at hl <;> cases hl <;> rfl

/-- **Any other beat latches** (#293: `Held`, or `Acquired` at a token other
than the handle's): the fence, the wake-up, and the heartbeat stops. A free
row is re-taken at the next token first. -/
theorem beat_latches {i : Nat} {h : H} (hl : ldHandle (s.slots i) = some h)
    (hb : h.beating = true) (ho : ownGen (s.rows i) h = false) :
    ∃ s', evBeat s i = some s' ∧
      ldHandle (s'.slots i) = some { h with fenced := true, woken := true, beating := false } := by
  unfold evBeat
  simp only [hl, hb, ho]
  refine ⟨_, rfl, ?_⟩
  simp only [upd_same]
  exact ldHandle_mapLD hl _

/-- A flush the store refuses as stale is never on the handle's own
generation, so the next beat latches with the wake-up (#293: "serve is woken
by the next heartbeat"). -/
theorem stale_not_own_gen {r : Row} {h : H} (hs : h.token < r.token ∨ r.holder = .tomb) :
    ownGen r h = false := by
  unfold ownGen
  rcases hs with hs | hs
  · simp; omega
  · simp [hs]

/-- **The stale-write latch is quiet** (#293): it sets the fence alone,
without the wake-up. -/
theorem stale_quiet {i : Nat} {h : H} (hl : ldHandle (s.slots i) = some h)
    (he : evStale s i = some s') : ldHandle (s'.slots i) = some { h with fenced := true } := by
  unfold evStale at he
  rw [hl] at he
  simp only at he
  split at he
  · cases he
    simp only [upd_same]
    exact ldHandle_mapLD hl _
  · cases he

/-- So a watcher already parked on the wake-up does not fire on it: the
session stays live (writes refused) until a beat wakes it. -/
theorem parked_needs_wake {i : Nat} {h : H} (hl : ldHandle (s.slots i) = some h)
    (hw : h.watch = .parked) (hk : h.woken = false) : evWatchFire s i = none := by
  unfold evWatchFire
  simp [hl, hw, hk]

/-- **A latch lets the watcher fire**: once the heartbeat has latched (fence
and wake-up), a watcher that has not fired yet fires, and the detach is
spawned. -/
theorem watcher_fires_after_latch {i : Nat} {h : H} (hl : ldHandle (s.slots i) = some h)
    (hw : h.watch = .unpolled ∨ h.watch = .parked) (hf : h.fenced = true) (hk : h.woken = true) :
    ∃ s', evWatchFire s i = some s' ∧ i ∈ s'.lost := by
  unfold evWatchFire
  have : (h.watch = .unpolled ∧ h.fenced = true) ∨ (h.watch = .parked ∧ h.woken = true) := by
    rcases hw with e | e
    · exact Or.inl ⟨e, hf⟩
    · exact Or.inr ⟨e, hk⟩
  simp only [hl, this, ↓reduceIte]
  exact ⟨_, rfl, List.mem_cons_self⟩

/-- The erase's own fence is never read as a lost lease: the erasing slot's
watcher was stopped before the fence (test
`erasing_an_attached_session_leaves_only_the_tombstone_and_nothing_recreates_it`). -/
theorem erase_fence_not_lost {i : Nat} {eh : Option H} {q : Prior}
    (he : s.slots i = some (.erasing eh q)) : evWatchFire s i = none := by
  unfold evWatchFire
  simp [he, ldHandle]

/-- **A dropped flight's release can take a later attach's lease.**
`Flight`'s `Drop` removes the slot first and spawns `release_abandoned` after
it, holder-scoped by the process's holder, which every handle of the process
shares. If the session is attached again before that task runs, the release
lets go of the new handle's lease, and the new handle's next beat re-takes
the row at the next token and (since #293) fences the healthy session. -/
theorem stale_release_fences {i : Nat} {h : H} (hl : s.slots i = some (.live h))
    (hrow : s.rows i = ⟨.me, h.token⟩) (hb : h.beating = true) (ha : i ∈ s.aband) :
    ∃ s1 s2, evAbandRun s i = some s1 ∧ evBeat s1 i = some s2 ∧
      s1.rows i = ⟨.released, h.token⟩ ∧
      s2.slots i = some (.live { h with fenced := true, woken := true, beating := false }) ∧
      s2.rows i = ⟨.me, h.token + 1⟩ := by
  let s1 : St := { s with aband := s.aband.erase i, rows := upd s.rows i (relMe (s.rows i)) }
  have h1 : evAbandRun s i = some s1 := by
    unfold evAbandRun; simp [ha, s1]
  have hrow1 : s1.rows i = ⟨.released, h.token⟩ := by simp [s1, hrow, relMe]
  have hl1 : s1.slots i = some (.live h) := hl
  let s2 : St := { s1 with
    slots := upd s1.slots i (some (.live { h with fenced := true, woken := true, beating := false })),
    rows := upd s1.rows i ⟨.me, h.token + 1⟩ }
  have h2 : evBeat s1 i = some s2 := by
    unfold evBeat
    simp only [hl1, ldHandle, hb, hrow1, ownGen, mapLD]
    simp [s2]
  exact ⟨s1, s2, h1, h2, hrow1, by simp [s2], by simp [s2]⟩

/-! ### The pinned retry -/

/-- `due()` is exactly the pinned sessions held elsewhere whose `retry_at`
has come. -/
theorem mem_due {j : Nat} :
    j ∈ due c s ↔ j ∈ c.pinned ∧ ∃ r p, s.slots j = some (.held r p) ∧ r ≤ s.now := by
  unfold due
  rw [List.mem_filter]
  constructor
  · rintro ⟨hp, hd⟩
    refine ⟨hp, ?_⟩
    revert hd
    cases e : s.slots j with
    | none => simp
    | some x => cases x <;> simp
  · rintro ⟨hp, r, p, hs, hr⟩
    exact ⟨hp, by simp [hs, hr]⟩

/-- **A session awaiting its previous handle is not attached again**
(`awaiting_previous`, #32; test
`a_detached_session_s_handle_is_gone_before_it_is_attached_again`): while the
detached handle lives and `PREVIOUS_HANDLE_WAIT` has not passed, the retry
does not acquire; it pushes `retry_at` back by `ELECTION_RETRY` and keeps no
permit. -/
theorem retry_waits_previous {i r k t0 : Nat} (hr : s.retry = some (i, false))
    (hcl : s.closing = false) (hf : s.free ≠ 0) (hs : s.slots i = some (.held r (some (k, t0))))
    (hk : k ∈ s.strays) (ht : s.now < t0 + c.wait) :
    evRPermit c s = some { s with slots := upd s.slots i (some (.held (s.now + c.er) (some (k, t0)))),
                                  retry := none } := by
  unfold evRPermit
  simp [hr, hcl, hf, hs, hk, ht]

/-- The same for an on-demand session (`awaiting_previous_on_demand`): the
attach ends 503 and takes no permit. -/
theorem attach_waits_previous {i : Nat} {cr : Bool} (hs : s.slots i = some (.attaching cr .queued))
    (hcl : s.closing = false) (hf : s.free ≠ 0) (ha : awaitingOD c s i = true) :
    evFPermit c s i = some (endFlight s i 0) := by
  unfold evFPermit
  simp [hs, hcl, hf, ha]

theorem replaceHeld_at {f : Nat → Option Slot} {i : Nat} {x : Slot} :
    replaceHeld f i x i = f i ∨ replaceHeld f i x i = some x := by
  unfold replaceHeld
  cases e : f i with
  | none => left; simp [e]
  | some y =>
    cases y <;> simp [e]

/-- Where the retry's last write leaves its slot: as it was, live, due again
in `PINNED_RETRY`, `Erased` or `Failed`. -/
theorem rResult_slot {i : Nat} {o : ROut} (hr : s.retry = some (i, true))
    (he : evRResult c s o = some s') :
    s'.slots i = s.slots i ∨ (∃ h, s'.slots i = some (.live h)) ∨
      s'.slots i = some (.held (s.now + c.pr) none) ∨ s'.slots i = some .erased ∨
      s'.slots i = some .failed := by
  unfold evRResult at he
  rw [hr] at he
  have rh (x : Slot) := replaceHeld_at (f := s.slots) (i := i) (x := x)
  cases o with
  | acquired =>
    simp only at he
    split at he
    · split at he
      · cases he; exact Or.inr (Or.inl ⟨_, upd_same _ _ _⟩)
      · cases he; exact Or.inl rfl
    · cases he
  | held =>
    simp only at he
    split at he
    · cases he
      rcases rh (.held (s.now + c.pr) none) with e | e
      · exact Or.inl e
      · exact Or.inr (Or.inr (Or.inl e))
    · cases he
  | transient =>
    simp only at he
    cases he
    rcases rh (.held (s.now + c.pr) none) with e | e
    · exact Or.inl e
    · exact Or.inr (Or.inr (Or.inl e))
  | erased =>
    simp only at he
    split at he
    · cases he
      rcases rh .erased with e | e
      · exact Or.inl e
      · exact Or.inr (Or.inr (Or.inr (Or.inl e)))
    · cases he
  | permanent =>
    simp only at he
    split at he
    · cases he
    · cases he
      rcases rh .failed with e | e
      · exact Or.inl e
      · exact Or.inr (Or.inr (Or.inr (Or.inr e)))
  | shutdown =>
    simp only at he
    cases he; exact Or.inl rfl
  | timeout =>
    simp only at he
    cases he
    rcases rh (.held (s.now + c.pr) none) with e | e
    · exact Or.inl e
    · exact Or.inr (Or.inr (Or.inl e))
  | abandoned =>
    simp only at he
    cases he; exact Or.inl rfl

/-- **Retry backoff** (design §3.2): every `HeldElsewhere` the retry writes
after an attempt is due `PINNED_RETRY` later. -/
theorem retry_backoff {i : Nat} {o : ROut} {r : Nat} {q : Option (Nat × Nat)}
    (hr : s.retry = some (i, true)) (he : evRResult c s o = some s')
    (hs : s'.slots i = some (.held r q)) : s'.slots i = s.slots i ∨ r = s.now + c.pr := by
  rcases rResult_slot hr he with e | ⟨h, e⟩ | e | e | e
  · exact Or.inl e
  · rw [e] at hs; cases hs
  · rw [e] at hs; cases hs; exact Or.inr rfl
  · rw [e] at hs; cases hs
  · rw [e] at hs; cases hs

/-! ### Attaches end: filled or released -/

/-- **A reserved place is never leaked when the attach is cancelled**
(`Flight`'s `Drop`, #32; test
`an_attach_that_dies_answers_its_waiters_and_frees_its_place`): an attach task
that panics or is aborted in any phase removes its slot and gives its permit
back. -/
theorem drop_clears {i : Nat} {cr : Bool} {p : Phase} (hs : s.slots i = some (.attaching cr p)) :
    ∃ s', evFDrop s i = some s' ∧ s'.slots i = none ∧ s'.free = s.free + p.holds ∧ i ∈ s'.aband := by
  unfold evFDrop
  simp only [hs]
  exact ⟨_, rfl, by simp [endFlight], rfl, List.mem_cons_self⟩

/-- Nor when it times out (`ATTACH_TIMEOUT`, #32; test
`an_attach_that_hangs_times_out_and_does_not_stick`). -/
theorem timeout_clears {i : Nat} {cr : Bool} {p : Phase} (hs : s.slots i = some (.attaching cr p)) :
    ∃ s', evFTimeout s i = some s' ∧ s'.slots i = none ∧ s'.free = s.free + p.holds := by
  unfold evFTimeout
  simp only [hs]
  split
  · exact ⟨_, rfl, by simp [endFlight], rfl⟩
  · exact ⟨_, rfl, by simp [endFlight], rfl⟩

/-- An attach's admission fills its place or releases it: the slot ends live
or absent. -/
theorem admit_fills_or_releases {i : Nat} {ok : Bool} (he : evFAdmit c s i ok = some s') :
    (∃ h, s'.slots i = some (.live h)) ∨ s'.slots i = none := by
  unfold evFAdmit at he
  split at he
  · split at he
    · cases he; right; simp [endFlight]
    · split at he
      · cases he; left; exact ⟨_, upd_same _ _ _⟩
      · cases he; right; simp [endFlight]
  · cases he

/-- The tombstone is never admitted: an acquire meets it as held. -/
theorem tomb_never_admitted {i : Nat} {ok : Bool} (ht : (s.rows i).holder = .tomb)
    (he : evFAdmit c s i ok = some s') : s'.slots i = none := by
  unfold evFAdmit at he
  split at he
  · simp only [acq, ht] at he
    cases he; simp [endFlight]
  · cases he

/-! ### The erase -/

/-- The erase claims an attached session by fencing its handle quietly and
stopping its watcher first, so the fence is not read as a lost lease. -/
theorem eClaim_fences {i : Nat} {h : H} (hl : s.slots i = some (.live h))
    (he : evEClaim c s i = some s') :
    s'.slots i = some (.erasing (some { h with fenced := true, watch := .off, beating := false })
      .absent) := by
  unfold evEClaim at he
  split at he
  · simp only [hl] at he
    cases he; simp
  · cases he

/-- An erase of a session being attached, detached or erased is answered 503
and claims nothing (test `an_erase_of_an_attaching_session_is_503`,
`an_erase_of_a_detaching_session_is_503`). -/
theorem eClaim_refuses_busy {i : Nat}
    (hb : (∃ cr p, s.slots i = some (.attaching cr p)) ∨ (∃ h w, s.slots i = some (.detaching h w)) ∨
      (∃ eh q, s.slots i = some (.erasing eh q))) : evEClaim c s i = none := by
  unfold evEClaim
  split
  · rcases hb with ⟨cr, p, e⟩ | ⟨h, w, e⟩ | ⟨eh, q, e⟩ <;> simp [e]
  · rfl

/-- A committed erase leaves the tombstone, and the slot `Erased` (pinned) or
removed (on demand). -/
theorem eEnd_commit {i : Nat} {h : H} {q : Prior}
    (hs : s.slots i = some (.erasing (some h) q)) (hrow : (s.rows i).holder = .me) :
    ∃ s', evEEnd c s i false = some s' ∧ (s'.rows i).holder = .tomb ∧
      s'.slots i = (if isPinned c i = true then some .erased else none) := by
  unfold evEEnd
  simp only [hs, hrow]
  refine ⟨_, rfl, by simp, ?_⟩
  unfold markErased
  split <;> simp_all

end Local

/-! ## The pure pieces -/

section Pure

/-! ### Which serves detach a session on a lost lease -/

inductive Policy where
  | exitProcess
  | detachSession
  deriving DecidableEq, Repr

/-- `LeaseLossPolicy::for_scope`: one pinned session and nothing on demand
exits the process on a lost lease; anything more detaches the session. -/
def forScope (pinned : Nat) (onDemand : Bool) : Policy :=
  if pinned > 1 ∨ onDemand = true then .detachSession else .exitProcess

theorem forScope_exit_iff (p : Nat) (od : Bool) :
    forScope p od = .exitProcess ↔ p ≤ 1 ∧ od = false := by
  unfold forScope
  split
  · rename_i h
    constructor
    · intro e; cases e
    · intro ⟨h1, h2⟩; rcases h with h | h
      · omega
      · rw [h2] at h; cases h
  · rename_i h
    constructor
    · intro _
      constructor
      · omega
      · cases od
        · rfl
        · exact absurd (Or.inr rfl) h
    · intro _; rfl

/-- The stdio cap (`too_many_stdio_sessions`, exit 2): a stdio serve takes at
most one `--session`. -/
def stdioAccepts (sessions : Nat) : Bool := decide (sessions ≤ 1)

/-- A stdio serve's registry is a registry of one under `ExitProcess`: no
watcher, and `spawn_retry_loop` starts nothing (test
`one_pinned_session_exits_on_lease_loss_and_more_detach`). -/
theorem stdio_exits_on_loss {n : Nat} (h : stdioAccepts n = true) :
    forScope n false = .exitProcess := by
  unfold stdioAccepts at h
  exact (forScope_exit_iff n false).2 ⟨by simpa using h, rfl⟩

example : forScope 1 false = .exitProcess := by decide
example : forScope 2 false = .detachSession := by decide
example : forScope 1 true = .detachSession := by decide
example : stdioAccepts 2 = false := by decide

/-! ### Constants -/

/-- `PINNED_RETRY` in milliseconds: five `ELECTION_RETRY`s, never under a
second.
Pinned by `formal_constants_tests::registry`. -/
def pinnedRetry (er : Nat) : Nat := if 5 * er < 1000 then 1000 else 5 * er

/-- `PINNED_RETRY` is at least a second and at least `ELECTION_RETRY`
(test `the_pinned_retry_is_five_election_retries_and_at_least_a_second`): a
sub-second `ELECTION_RETRY` cannot make the retry loop spin. -/
theorem pinnedRetry_ge (er : Nat) :
    1000 ≤ pinnedRetry er ∧ 5 * er ≤ pinnedRetry er ∧ er ≤ pinnedRetry er := by
  unfold pinnedRetry
  split <;> omega

example : pinnedRetry 1000 = 5000 := by decide
example : pinnedRetry 0 = 1000 := by decide
example : pinnedRetry 150 = 1000 := by decide

/-- `unplaced_cap`: `UNPLACED_PER_PLACE × max_attached`, at least 1.
Pinned by `formal_constants_tests::registry`. -/
def unplacedCap (maxAttached : Nat) : Nat := max (maxAttached * 2) 1

theorem unplacedCap_pos (m : Nat) : 1 ≤ unplacedCap m := by
  unfold unplacedCap; omega

/-- The test `the_unplaced_cap_is_twice_max_attached_and_at_least_one`. -/
example : unplacedCap 16 = 32 ∧ unplacedCap 1 = 2 ∧ unplacedCap 0 = 1 := by decide

/-- `place_share` (`http_guards::credential_share`): the places divided
among the credentials, rounded down, at least 1. -/
def placeShare (places creds : Nat) : Nat := max (places / max creds 1) 1

theorem placeShare_pos (p n : Nat) : 1 ≤ placeShare p n := by
  unfold placeShare; omega

/-- Rounded down, the shares never add up to more than the places while there
are no more credentials than places (#32: "every credential can
always reach its share by eviction, whatever the others hold"). -/
theorem placeShare_fair {p n : Nat} (h1 : 1 ≤ n) (h2 : n ≤ p) : placeShare p n * n ≤ p := by
  unfold placeShare
  have hn : max n 1 = n := by omega
  rw [hn]
  have hq : 1 ≤ p / n := (Nat.le_div_iff_mul_le (by omega)).2 (by omega)
  have hm : max (p / n) 1 = p / n := by omega
  rw [hm]
  exact Nat.div_mul_le_self p n

/-- The test `the_place_share_is_pr5s_rule`. -/
example : placeShare 15 1 = 15 ∧ placeShare 15 2 = 7 ∧ placeShare 15 4 = 3 ∧
    placeShare 2 5 = 1 ∧ placeShare 0 3 = 1 := by decide

/-! ### `choose_victim` -/

/-- An on-demand slot as `choose_victim` reads it: its id, whether it is
pinned and live, its calls in flight, how long it has been idle (ms) and the
credential that attached it. -/
structure Cand where
  id : Nat
  pinned : Bool
  live : Bool
  inflight : Nat
  idle : Nat
  owner : Nat
  deriving DecidableEq, Repr

/-- `choose_victim`'s filter: live, not pinned, nothing in flight, idle at
least `min_idle`, and the requester's own, or, while the requester holds less
than its share, a session of a credential over its share. `held` is
`held_by`. -/
def eligible (held : Nat → Nat) (req share minIdle : Nat) (x : Cand) : Bool :=
  !x.pinned && x.live && x.inflight == 0 && decide (minIdle ≤ x.idle) &&
    (x.owner == req || (decide (held req < share) && decide (share < held x.owner)))

/-- `max_by(a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))`: `a` beats `b` when it
has been idle longer, or as long with a smaller id. -/
def beats (a b : Cand) : Bool := decide (b.idle < a.idle) || (a.idle == b.idle && decide (a.id < b.id))

def pick (acc : Option Cand) (x : Cand) : Option Cand :=
  match acc with
  | none => some x
  | some a => if beats x a = true then some x else some a

/-- `on_demand::choose_victim`. -/
def chooseVictim (held : Nat → Nat) (req share minIdle : Nat) (cs : List Cand) : Option Cand :=
  (cs.filter (eligible held req share minIdle)).foldl pick none

theorem foldl_pick_some : ∀ (l : List Cand) (a : Cand), ∃ v, l.foldl pick (some a) = some v
  | [], a => ⟨a, rfl⟩
  | x :: l, a => by
    simp only [List.foldl, pick]
    split
    · exact foldl_pick_some l x
    · exact foldl_pick_some l a

theorem foldl_pick_spec : ∀ (l : List Cand) (acc : Option Cand) (v : Cand),
    l.foldl pick acc = some v →
      (acc = some v ∨ v ∈ l) ∧ (∀ x ∈ l, x.idle ≤ v.idle) ∧ (∀ a, acc = some a → a.idle ≤ v.idle)
  | [], acc, v, h => by
    simp only [List.foldl] at h
    exact ⟨Or.inl h, by simp, fun a ha => by rw [h] at ha; cases ha; exact Nat.le_refl _⟩
  | x :: l, acc, v, h => by
    simp only [List.foldl] at h
    obtain ⟨hm, hl, ha⟩ := foldl_pick_spec l (pick acc x) v h
    have hx : ∀ r, pick acc x = some r → x.idle ≤ r.idle ∧ (∀ a, acc = some a → a.idle ≤ r.idle) ∧
        (r = x ∨ acc = some r) := by
      intro r hr
      unfold pick at hr
      split at hr
      · cases hr; exact ⟨Nat.le_refl _, (fun a e => by cases e), Or.inl rfl⟩
      · rename_i a
        split at hr
        · rename_i hb
          cases hr
          unfold beats at hb
          simp at hb
          refine ⟨Nat.le_refl _, (fun a' e => by cases e; omega), Or.inl rfl⟩
        · rename_i hb
          cases hr
          unfold beats at hb
          simp at hb
          refine ⟨by omega, (fun a' e => by cases e; exact Nat.le_refl _), Or.inr rfl⟩
    obtain ⟨r, hr⟩ : ∃ r, pick acc x = some r := by
      unfold pick; split
      · exact ⟨_, rfl⟩
      · split <;> exact ⟨_, rfl⟩
    obtain ⟨hxr, har, hor⟩ := hx r hr
    have hrv := ha r hr
    refine ⟨?_, ?_, ?_⟩
    · rcases hm with e | e
      · rw [hr] at e; cases e
        rcases hor with e' | e'
        · exact Or.inr (e' ▸ List.mem_cons_self)
        · exact Or.inl e'
      · exact Or.inr (List.mem_cons_of_mem _ e)
    · intro y hy
      rcases List.mem_cons.1 hy with e | e
      · subst e; omega
      · exact hl y e
    · intro a e
      have := har a e
      omega

/-- The victim is one of the candidates, and passes the filter: never pinned,
never one with a call in flight or used within `min_idle`. -/
theorem chooseVictim_eligible {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v : Cand} (h : chooseVictim held req share minIdle cs = some v) :
    v ∈ cs ∧ eligible held req share minIdle v = true := by
  obtain ⟨hm, -, -⟩ := foldl_pick_spec _ none v h
  rcases hm with e | e
  · cases e
  · exact List.mem_filter.1 e

/-- `Full` (503) exactly when no candidate passes the filter. -/
theorem chooseVictim_none_iff {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand} :
    chooseVictim held req share minIdle cs = none ↔
      ∀ x ∈ cs, eligible held req share minIdle x = false := by
  unfold chooseVictim
  constructor
  · intro h x hx
    cases e : eligible held req share minIdle x
    · rfl
    · exfalso
      have hmem : x ∈ cs.filter (eligible held req share minIdle) := List.mem_filter.2 ⟨hx, e⟩
      obtain ⟨l1, l2, hsplit⟩ := List.append_of_mem hmem
      rw [hsplit, List.foldl_append] at h
      simp only [List.foldl] at h
      obtain ⟨v, hv⟩ := foldl_pick_some l2 (match l1.foldl pick none with
        | none => x
        | some a => if beats x a = true then x else a)
      have : pick (l1.foldl pick none) x = some (match l1.foldl pick none with
          | none => x
          | some a => if beats x a = true then x else a) := by
        unfold pick; split
        · rfl
        · split <;> rfl
      rw [this, hv] at h
      cases h
  · intro h
    have : cs.filter (eligible held req share minIdle) = [] :=
      List.filter_eq_nil_iff.2 (fun x hx => by simp [h x hx])
    rw [this]; rfl

/-- The least recently used of the eligible: none of them has been idle
longer. -/
theorem chooseVictim_max {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v x : Cand} (h : chooseVictim held req share minIdle cs = some v) (hx : x ∈ cs)
    (he : eligible held req share minIdle x = true) : x.idle ≤ v.idle := by
  obtain ⟨-, hl, -⟩ := foldl_pick_spec _ none v h
  exact hl x (List.mem_filter.2 ⟨hx, he⟩)

/-- **A credential at or over its share evicts only its own sessions** (#32;
test `a_credential_at_its_share_evicts_only_its_own_sessions`). -/
theorem at_share_evicts_own {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v : Cand} (hs : share ≤ held req) (h : chooseVictim held req share minIdle cs = some v) :
    v.owner = req := by
  have he := (chooseVictim_eligible h).2
  unfold eligible at he
  simp at he
  rcases he.2 with e | ⟨e, -⟩
  · exact e
  · omega

example : chooseVictim (fun _ => 1) 7 1 2000
    [⟨1, false, true, 0, 5000, 7⟩, ⟨2, false, true, 0, 9000, 8⟩, ⟨3, false, true, 0, 3000, 7⟩] =
    some ⟨1, false, true, 0, 5000, 7⟩ := by decide

example : chooseVictim (fun o => if o = 8 then 3 else 0) 7 1 2000
    [⟨1, false, true, 0, 5000, 7⟩, ⟨2, false, true, 0, 9000, 8⟩, ⟨4, true, true, 0, 99000, 7⟩,
     ⟨5, false, true, 1, 99000, 7⟩] =
    some ⟨2, false, true, 0, 9000, 8⟩ := by decide

/-! ### What a request gets: `answer_for` and `/admin/sessions` -/

/-- `Lookup`, with `Retry-After` in milliseconds. -/
inductive Lookup where
  | live
  | unavailable (retryAfter : Nat)
  | failed
  | erased
  | notHosted
  deriving DecidableEq, Repr

/-- `answer_for`, and `lookup`'s answer for a slot-less id. -/
def answer (pinned : Bool) (now : Nat) : Option Slot → Lookup
  | some (.live _) => .live
  | some (.attaching _ _) => .unavailable 1000
  | some (.detaching _ _) => .unavailable 1000
  | some (.held r _) => .unavailable (max (r - now) 1000)
  | some .failed => .failed
  | some (.erasing _ _) => .unavailable 1000
  | some .erased => .erased
  | none => if pinned then .unavailable 1000 else .notHosted

/-- Every 503 carries a `Retry-After` of at least a second, except `Failed`'s,
which has none (the table in the module docs). -/
theorem answer_retry_after_ge {pinned : Bool} {now r : Nat} {x : Option Slot}
    (h : answer pinned now x = .unavailable r) : 1000 ≤ r := by
  unfold answer at h
  split at h <;> (try split at h) <;> cases h <;> omega

/-- **A session still being erased is 503, never the erased 410** (#32;
test `an_erasing_session_is_503_and_only_an_erased_one_is_410`):
the permanent answer waits for the commit. -/
theorem answer_erasing (pinned : Bool) (now : Nat) (eh : Option H) (q : Prior) :
    answer pinned now (some (.erasing eh q)) = .unavailable 1000 := rfl

/-- Only a live slot is served, and a slot-less id is the uniform 404 only
when it is not pinned. -/
theorem answer_live_iff {pinned : Bool} {now : Nat} {x : Option Slot} :
    answer pinned now x = .live ↔ ∃ h, x = some (.live h) := by
  unfold answer
  split <;> simp
  split <;> simp

/-- `SlotView::state`. -/
def stateName : Option Slot → String
  | some (.attaching _ _) => "attaching"
  | some (.live _) => "live"
  | some (.detaching _ _) => "detaching"
  | some (.held _ _) => "held_elsewhere"
  | some .failed => "failed"
  | some (.erasing _ _) => "erasing"
  | some .erased => "erased"
  | none => "unattached"

/-- Insertion sort on ids, for the sorted on-demand rows. -/
def insertId (x : Nat) : List Nat → List Nat
  | [] => [x]
  | y :: l => if x ≤ y then x :: y :: l else y :: insertId x l

def sortIds : List Nat → List Nat
  | [] => []
  | x :: l => insertId x (sortIds l)

theorem mem_insertId {x y : Nat} : ∀ {l : List Nat}, y ∈ insertId x l ↔ y = x ∨ y ∈ l
  | [] => by simp [insertId]
  | z :: l => by
    unfold insertId
    split
    · simp
    · simp only [List.mem_cons, mem_insertId (l := l)]
      constructor
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)

theorem mem_sortIds {y : Nat} : ∀ {l : List Nat}, y ∈ sortIds l ↔ y ∈ l
  | [] => by simp [sortIds]
  | x :: l => by simp [sortIds, mem_insertId, mem_sortIds (l := l)]

/-- `slot_views`: a row for every pinned session in pinned order, then for
every other id holding a slot, sorted; each row is `(session, state, pinned)`. -/
def slotViews (c : Cfg) (f : Nat → Option Slot) : List (Nat × String × Bool) :=
  (c.pinned ++ sortIds (c.ids.filter (fun j => !isPinned c j && (f j).isSome))).map
    (fun j => (j, stateName (f j), isPinned c j))

/-- The pinned sessions come first, in pinned order. -/
theorem slotViews_pinned_first (c : Cfg) (f : Nat → Option Slot) :
    ((slotViews c f).map (·.1)).take c.pinned.length = c.pinned := by
  unfold slotViews
  simp [Function.comp_def]

/-- Every id the registry holds a slot for has its row, with its state. -/
theorem slotViews_complete {c : Cfg} {s : St} (hI : Inv c s) {j : Nat} {x : Slot}
    (hj : s.slots j = some x) : (j, stateName (some x), isPinned c j) ∈ slotViews c s.slots := by
  unfold slotViews
  rw [List.mem_map]
  refine ⟨j, ?_, by rw [hj]⟩
  rw [List.mem_append]
  by_cases hp : isPinned c j = true
  · left; simpa [isPinned] using hp
  · right
    rw [mem_sortIds, List.mem_filter]
    exact ⟨mem_of_some hI hj, by simp [hj]; simpa using hp⟩

/-! ### The negative cache and how an attach ends -/

theorem negGet_create_not_absent (e : Option (Neg × Nat)) (now ttl : Nat) :
    negGet e now ttl true ≠ some .absent := by
  unfold negGet
  split
  · simp
  · split
    · simp
    · split <;> simp_all

theorem negGet_expired (n : Neg) {t0 now ttl : Nat} (cr : Bool) (h : t0 + ttl ≤ now) :
    negGet (some (n, t0)) now ttl cr = none := by
  simp [negGet, h]

/-- The test `the_negative_cache_expires_skips_absent_for_create_and_is_bounded`
(its first half). -/
example : negGet (some (.erased, 0)) 10 30000 true = some .erased ∧
    negGet (some (.absent, 0)) 10 30000 false = some .absent ∧
    negGet (some (.absent, 0)) 10 30000 true = none ∧
    negGet (some (.erased, 0)) 30000 30000 false = none := by decide

/-- `AttachOutcome`. -/
inductive Outcome where
  | attached
  | absent
  | erased
  | busy
  | failed
  deriving DecidableEq, Repr

/-- `Flight::end`'s outcome: the drop guard's `Failed` on a slot already
live is `Attached` (#32). -/
def endOutcome (o : Outcome) (slotLive : Bool) : Outcome :=
  if o = .failed ∧ slotLive = true then .attached else o

/-- What `Flight::end` puts in the negative cache, if anything: `create` is
the flight's capability, `mine` whether its slot was still its own. -/
def negPut (o : Outcome) (create mine : Bool) : Option Neg :=
  match o with
  | .absent => if create = true then none else some .absent
  | .erased => some .erased
  | .failed => if mine = true then some .failed else none
  | _ => none

/-- Held elsewhere, transient errors, the closing serve and a timeout are never
cached: the next request tries again (#32). -/
theorem busy_never_cached (create mine : Bool) : negPut .busy create mine = none := rfl

/-- **A panic after admission does not block the session** (test
`a_panic_after_admission_does_not_block_the_session`): the drop guard finds
the slot live, its waiters are told `Attached`, and nothing is cached. -/
theorem panic_after_admit (create mine : Bool) :
    endOutcome .failed true = .attached ∧ negPut (endOutcome .failed true) create mine = none := by
  simp [endOutcome, negPut]

end Pure

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

theorem cfgEx_wf : WF cfgEx :=
  ⟨by decide, by decide⟩

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

/-! ### `watch_lease` detaches by session id -/

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
    .detachRun 1, .strayDrop 0, .rowLapse 1] ++ attach1 ++ [.enter 1, .lostRun 1]

/-- **A lost lease's detach can take down the next, healthy attach.**
`watch_lease` spawns `spawn_detach(id, LeaseLost)` and `detach` takes
whatever is live under that id (`lostRun_any_live`). If the detach task runs
only after the session has been detached by another path and attached again,
it detaches the new handle: unfenced, with a call in flight, for a lease it
never lost. The safety of writes is not at stake (the handle's close flushes
and releases), but the call is cut and the session is detached for nothing.
It needs the spawned task to be delayed past a whole detach and attach. -/
theorem lost_detach_takes_healthy :
    ∃ s, Reach cfgEx s ∧ s.slots 1 = some (.detaching hSecond none) := by
  obtain ⟨s, hs, hp⟩ := reach_of_run (Reach.init s0_init) (es := abaRun)
    (p := fun s => decide (s.slots 1 = some (.detaching hSecond none))) (by decide)
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
example : (run cfgEx s0 (staleRun ++ [.beat 1, .watchFire 1])).any (fun s => s.lost == [1]) = true := by
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

theorem sHeld_init : Init cfgEx sHeld := by
  refine ⟨rfl, rfl, rfl, fun j => ?_⟩
  by_cases hj : j = 0
  · subst hj
    exact Or.inr ⟨by decide, Or.inr (Or.inl ⟨0, rfl⟩)⟩
  · left
    simp [sHeld, s0, hj]

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
  [.rowLapse 0, .beat 0, .watchFire 0, .lostRun 0, .detachRun 0, .eClaim 0, .eEnd 0 false]

example : (run cfgEx s0 zombieRun).any
    (fun s => isHeld (s.slots 0) && s.rows 0 == ⟨.me, 2⟩ && s.rel == []) = true := by decide

/-- And the pinned retry then takes the re-taken row by the refresh arm, at
that same token: a new, healthy handle serves the session again. This is
fine: leases.rs only says the fenced handle's heartbeat stops. -/
example : (run cfgEx s0 [.rowLapse 0, .beat 0, .watchFire 0, .lostRun 0, .detachRun 0,
    .strayDrop 100, .tick 5000, .rRound, .rStart, .rPermit, .rResult .acquired]).any
    (fun s => s.rows 0 == ⟨.me, 2⟩ && match s.slots 0 with
      | some (.live h) => h.token == 2 && !h.fenced
      | _ => false) = true := by decide

/-! ### Backoff and the previous handle -/

/-- The pinned session loses its lease and is detached: `HeldElsewhere`, due in
`PINNED_RETRY`, its handle `previous`. -/
def pinLost : List Ev :=
  [.rowLapse 0, .otherTake 0, .beat 0, .watchFire 0, .lostRun 0, .detachRun 0]

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
