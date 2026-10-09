# #32 PR 6: on-demand sessions (decisions)

Base: main `5773e4a` (PRs 1 to 5 and 8 merged). Design of record: the
approved #32 design, sections 1, 3.2, 3.4, 3.6, 4 and 6.1, and the PR 6 row
of section 8, which is the acceptance list. Decisions and why; the commits
carry the mechanics.

## Module map

| module | holds |
|---|---|
| `mcp/serve/activity.rs` (new) | `SessionActivity` (last use, tool calls in flight, on tokio's clock) and its `InFlight` guard |
| `mcp/server.rs` | `LamboServer::with_activity`; `call_tool` holds an `InFlight` for the whole call |
| `mcp/serve/registry.rs` | `Slot::Attaching`, `get_or_attach` / `route_or_start` / `attach_on_demand`, `RegistryBounds` / `OnDemandBounds`, the attach permits, `run_detach` (split from `detach`), `DetachReason`, the idle sweeper, `least_recently_used_idle`, `Lookup::Erased`, `LeaseLossPolicy::for_scope` |
| `mcp/serve/session.rs` | `AttachedSession.activity` and `.rate` (the per-session bucket), `admits_request` |
| `mcp/serve/transport.rs` | `serve_session` asks `get_or_attach` and draws the session's bucket |
| `mcp/serve/authority.rs` | `reaches_past_pinned`; the startup warnings as PR 6 leaves them |
| `mcp/serve.rs` | `SessionBounds` on `ServeOptions`; the policy derivation; `serve_pinned` takes the on-demand flag |

## Decisions

**When a serve attaches on demand.** Exactly when it is HTTP and some
configured credential's scope reaches past the pinned sessions: a
`session_prefix`, or an exact name that is not pinned
(`authority::reaches_past_pinned`). The legacy `default` and the implicit
`local` cover the pinned sessions only, and `"*"` reaches past them only
through another credential's prefix, so neither changes the answer. Such a
serve runs as a registry (`serve_pinned`) under `DetachSession` even with
one pinned session: `LeaseLossPolicy::for_scope(pinned, on_demand)` is the
one derivation point (design R4), as §4.2 says ("ExitProcess: one pinned
session and no on-demand scope"). Every other serve, the dogfood rig's one
`--session` with `LAMBO_AUTH_TOKEN` included, keeps PR 4's election path,
its proxy and `ExitProcess`, unchanged.

**Where the attach happens: after authorization.** `transport::serve_session`
calls `SessionRegistry::get_or_attach(id, grant.create)`, which is reached
only once the grant is authorized for the id (PR 5's order), so an
out-of-scope request still makes no store call. The existence probe
(`read_lease`) runs first for every on-demand attach, `create` or not: a
tombstone answers `Lookup::Erased` (410) before any acquire; no row and no
`create` is the uniform 404 (decision 3); otherwise `build_attach` through the
template, unchanged. One extra store read per attach is cheap and tells an
erased session apart without parsing an error string.

**Single flight.** `Slot::Attaching { done, create }` holds a
`watch::Receiver<Option<AttachOutcome>>`; every request for the session waits
on it, so N concurrent first requests cause one `build_attach` (tested by
counting `acquire_lease` and `read_lease` on a recording store). The attach
runs on its own spawned task, tracked with the detaches, so a client that
goes away cannot cancel it half way with a lease taken and not admitted. A
waiter whose credential has `create` does not take an `Absent` from a flight
that had none; it starts its own. A request follows a session through at
most three attaches (`ATTACH_FOLLOWS`) before answering 503, so a cap far too
small for its load cannot make a request spin.

**Attach concurrency.** `attach_concurrency` is a semaphore of permits
(`SessionBounds::attach_permits`: never 0, always 1 on SQLite, R1). It
replaces PR 4's `attach_lock` for the pinned background retry and the
on-demand attach alike. Each attach holds a permit across the acquire and
the admission; `close_set` sets `closing` and then takes every permit, so an
attach in flight is either in the set or has given up (raced against
`closed()`, releasing any lease it took, as PR 4's L8 does). No attach starts
once `closing` is set (§3.2 step 1).

**Capacity and eviction.** Places for on-demand sessions are
`max_attached - pinned`. Every on-demand slot that holds or is about to hold
a `Memory` (`Live`, `Attaching`, `Detaching`) takes one, so the cap holds
while sessions come and go. At the cap, the least recently used live
on-demand session with no tool call in flight is chosen and set to
`Detaching` under the slots' lock (so no other attach picks it and no request
reaches it), and the new attach's task detaches it before it builds: the
evicted lease is released before the new one is taken, and the number of
live `Memory`s never exceeds the cap. With none idle the answer is 503 and
`Retry-After: 5` (`AT_CAPACITY_RETRY`; the design gave no number). Pinned
sessions are never candidates.

**Detach ends by scope.** `detach` became `detach` (Live to Detaching, then
`run_detach`) and `run_detach` (the stages, unchanged). A pinned session
ends `HeldElsewhere` as in PR 4; an on-demand session's slot is removed, and
its previous handle (if anything still holds it) is kept weakly in
`previous`, so the next attach of that id answers 503 until it drops (bounded
by PR 4's `PREVIOUS_HANDLE_WAIT`), as the pinned retry waits. An idle or
evicted session's close flushes and releases its lease keeping the token
(#23), so a reattach mints `token + 1` and loads what it held; a lost
lease's close refuses, as in PR 4.

**Idle.** Defined by tool calls, not connections (R7). The router stamps
`last_used` on every request it routes to a live session, and
`LamboServer::call_tool` holds an `InFlight` guard for the whole call, which
covers the unix endpoint too. The sweeper runs every `min(30 s,
idle_detach)` (§3.4 says 30 s; a shorter `idle_detach_secs` sweeps at that
interval so a session is never kept more than twice its idle time) and
detaches each on-demand session idle at least `idle_detach` with nothing in
flight. Pinned sessions are never swept. The clock is tokio's, so the test
pauses it.

**Held elsewhere, on demand.** 503 with `Retry-After` = the time the
holder's lease could lapse (at least 1 s), and the slot is removed: no
background retry for an on-demand session (the next request tries again),
and no `lease:refused` ledger line, since this is the request path, not a
startup election.

**Per-session rate.** Each `AttachedSession` has its own `RateLimiter` at
`per_session_rps` (default `--rate-limit-rps`, burst 2x, 0 off), drawn in
`serve_session` after the guard's credential bucket and after the lookup, so
it is never an oracle. The answer is the guard's 429 with `Retry-After: 1`.
With one session and one credential at the default rate the two buckets
start full and drain together, so a single-session serve answers as before.
It composes with PR 5's per-credential bucket and share: a credential's
flood is bounded by its bucket, a session's flood by its own.

**Erased, on demand.** 410 with a plain body, no cached slot (each request
re-probes; one store read). PR 7 owns `Slot::Erased` and the negative cache,
and may replace this answer; see "For PR 7".

**Default session stays pinned.** PR 4 refused a non-pinned
`default_session`; that is kept (`pin_sessions`, whose message now says `/mcp`
serves a pinned session, never one attached on demand). Letting `/mcp` serve an on-demand session is not
in the acceptance row and would make `/mcp` attach.

**The notice.** `attach_concurrency`, `idle_detach_secs` and
`per_session_rps` are enforced over HTTP and do not apply to stdio, so
`unenforced_keys` lists only `[[serve.projects]]` over HTTP. The notice text
itself is unchanged (a test greps it).

**Refusal cursors.** The poller pruned every cursor whose session was not
pinned, which would have rebuilt an on-demand session's cursor every round
and re-booked a `LEASE_TTL` of refusals each time. A cursor is now kept while
its session is pinned or attached (own `fix` commit).

## Tests

- `mcp::serve::tests::registry::on_demand` (router and guards over a
  registry with a recording store, credentials `maker` with `create` and
  `reader` without): one `build_attach` for 8 concurrent first requests;
  eviction releases the lease, keeps its token, and a reattach mints
  `token + 1` with the same recall; idle detach on a paused clock and the
  pinned session untouched; 503 with `Retry-After` when nothing can be
  evicted (a session with a call in flight; the idle pinned one is not taken
  instead, and no store call is made) and when held elsewhere; a per-session
  flood gets 429 while another session is served; `create`-less attaches only
  an existing session (404 after one probe and no acquire otherwise); erased
  is 410 to both and never acquired; the shutdown set takes on-demand
  sessions (pinned first, then by id) and releases them; the permit count
  (SQLite 1, never 0) and the per-session rate default.
- `pinned_serve::one_pinned_session_with_a_prefix_credential_attaches_on_demand`:
  the real `serve_pinned_with`, one pinned session and a prefix credential,
  an on-demand attach, and the simulated SIGTERM releasing both leases.
- `activity` unit test (paused clock); `authority`:
  `a_scope_past_the_pinned_sessions_attaches_on_demand` and the rewritten
  startup-warning test; `for_scope` in the policy test.
- `tests/serve_table_unenforced.rs` now shows the notice from an HTTP serve
  (port 0, SIGTERM) and pins that a stdio serve with the bounds says nothing.
- Mutation checks: letting eviction pick the pinned session fails two
  on-demand tests; dropping the single-flight wait fails the concurrent-attach
  test.

## For PR 7

- Additive changes only: `Slot` gained `Attaching`; `Lookup` gained
  `Erased`; `DetachReason` (`LeaseLost`, `Idle`, `Evicted`) is new, and
  `spawn_detach` / `detach` take one, so PR 7's in-serve erase and operator
  detach add variants. `run_detach` takes a session already set
  `Detaching`; an erase that ends in `Erased` instead of removal or
  `HeldElsewhere` needs a third ending there.
- An on-demand attach of a tombstoned id answers 410 from the probe and keeps
  no slot. PR 7's `Slot::Erased` negative cache should replace that path
  (`attach_outcome`'s `is_tombstone` arm) and choose the MCP-route answer.
- `attach_permits` is the attach gate; an in-serve erase of an unattached
  session should take a permit too if it reads the store the same way.
