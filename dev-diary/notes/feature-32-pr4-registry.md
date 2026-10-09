# #32 PR 4: the session registry, `/mcp/s/{session}` routing, pinned multi-session (decisions)

Base: main `42a5ead` (PRs 1 to 3 merged). Design of record: the approved #32
design, sections 2 to 5 and 7, and the PR 4 row of section 8, which is the
acceptance list. Decisions and why; the commits carry the mechanics.

PR 4 serves **pinned** sessions only. Credentials (PR 5), on-demand attach,
LRU and idle detach (PR 6), the admin surface and in-serve erase (PR 7) and
the stdio cwd map (PR 8) build on it.

## Module map

| module | holds |
|---|---|
| `mcp/serve/registry.rs` (new) | `SessionRegistry`, `Slot` (`Live`, `Detaching`, `HeldElsewhere`), `Lookup`, `LeaseLossPolicy`, `SessionAttacher` (the template builder), `acquire` / `admit` / `mark_held`, the retry loop, `detach`, `close_set`, the lease watcher, `LiveSessions` over every session |
| `mcp/serve/pinned.rs` (new) | `pin_sessions` (the CLI's plan) and `check_pinned` (`serve`'s own check) |
| `mcp/serve/session.rs` | `AttachedSession` gains its own `StreamableHttpService` and `LocalSessionManager`; `SessionTasks` gains `lease_watcher` |
| `mcp/serve/transport.rs` | `serve_http(registry, ..)` and `session_router` (`/mcp`, `/mcp/s/{session}`) |
| `mcp/serve/process.rs` | `ProcessTasks::spawn(registry, ledger, heartbeat, keep_warm, embedder, agent, calibration)`; the heartbeat and refusal poller iterate the registry |
| `mcp/serve/shutdown.rs` | `registry_shutdown` (the `DetachSession` constructor of `HolderShutdown`), `stop_transport`, `book_lease_loss`, `SessionCloses::named` |
| `mcp/serve.rs` | `ServeOptions::sessions`; the one-session path (election, proxy) and `serve_pinned`; `close_holder`, the one stage tail both run |

## Decisions

**Every serve runs through a registry.** The design says `lambo serve
--session X` with no `[serve]` makes a registry of one pinned session under
`ExitProcess`. So it does, but the one-session path still attaches through
`resolve_role` (the election and J2's proxy are unchanged), then wraps the
holder in a registry of one. Two or more pinned sessions take
`serve_pinned`: no election, each session acquired from the one
`serve_builder` template (`template.clone().session(id)
.ledger(base.for_session(id)).endpoint(derived)`), so every session shares
the embedder, the store and PR 3's calibration. Both paths end in the same
`close_holder`, so the seven stages are one piece of code.

**The policy is derived in one place.** `LeaseLossPolicy::for_pinned(n)`:
one pinned session is `ExitProcess`, more is `DetachSession`. `HolderShutdown`
gained a second constructor, `registry_shutdown`, which has the two signal
arms and no fence arm; `serve` picks it only under `DetachSession`. The
one-session path still builds `holder_shutdown`, so JE2E-4's exit on a lost
lease is unchanged and the severing mutation is still a type error there.

**Lease loss under `DetachSession`.** Each session's watcher (in
`SessionTasks`, stopped at stage 5) waits on `lease_lost_latched`, books the
same `lease:lost` line `wind_down` books (`book_lease_loss`), warns, and
**spawns** `registry.detach`. Spawned, not awaited: the detach aborts the
watcher at its stage 5, and a detach running inside the task it aborts would
cancel itself. The slot is filled before the watcher starts, so a fence that
latches at once still finds the session live.

**Detach** (design §3.4): slot to `Detaching` (requests get 503,
`Retry-After: 1`), then under `ShutdownProgress::for_session`: stage 1 ends
the session's MCP sessions (`LocalSessionManager::close_session`, bounded by
`SHUTDOWN_GRACE`), stages 3 and 4 are `close_sessions(&[one], ..)
.named().report()`, stage 5 stops the watcher, stage 6 is
`release_endpoint`. Stages 2 and 7 are the process's and are skipped; the
calibration is never touched (PR 3). Then the slot becomes `HeldElsewhere`
and the retry loop takes it back. `SessionCloses::named()` exists because
PR 2's test pins that a set of one under `for_session` logs an unnamed
outcome; a detach names it explicitly instead, since an unattributed "tail
lost" in a multi-session process does not say whose.

**Held elsewhere is not an error and not a wait.** A pinned session another
writer holds at startup is `HeldElsewhere`: the others are served, its
requests get 503 with `Retry-After`, and the retry loop tries it every
`PINNED_RETRY` (5 s, `ELECTION_RETRY × 5`). The loser's `lease:refused`
line is booked once, at the transition, not on every retry. Any **other**
attach error (an unprovisioned store, a contract mismatch, an erased
session) refuses the start, after closing the sessions already acquired so
no lease is left to lapse. An erased pinned session is therefore a startup
error in PR 4 (as it is for a one-session serve); the `Erased` slot is PR 7's.

**Pinned only, so the default must be pinned.** PR 1's review asked PR 4/8
to decide what `/mcp` does when `default_session` is not hosted. With no
on-demand attach yet, `pin_sessions` refuses that at startup. Stdio owns
exactly one `--session` and ignores `[serve] sessions`; choosing a stdio
session without `--session` is PR 8's. The `max_attached` cap is re-checked
after the union of `--session` and `[serve] sessions` (PR 1 review). With
more than one pinned session every name must pass the strict addressed
charset (`check_pinned`, run by `serve` itself so a library caller meets it
too); one session keeps `--session`'s loose rule and is reachable at `/mcp`.

**Routing.** `/mcp` (the default) and `/mcp/s/{session}`, both `any`, both
behind the guards on `.layer` (PR 1's wire claim). The id is the raw path
segment, never percent-decoded, through `parse_addressed`; a refusal or an
unhosted id is `surface::session`'s uniform 404. Each session's request goes
to that session's own `StreamableHttpService::handle`. `/mcp/<anything
else>` no longer reaches a session (it did through `nest_service`); recorded
as Breaking in the changelog.

**One process-wide MCP-session cap.** `SessionRegistry` implements
`LiveSessions` by summing every attached session's manager, so
`HttpGuard.live` is the registry and nothing else in the guard changed.

**Process tasks iterate the registry, without reordering the startup.** The
heartbeat (one `stats` line per attached session, each through that
session's scoped ledger) and the refusal poller (one task, one
`RefusalCursor` per session, tick `max(500 ms, 100 ms × attached)`) iterate
`SessionRegistry::attached`. `ProcessTasks::spawn` still runs before the
session parts, so the one-session startup logs its lines in the old order;
both tasks wait on `SessionRegistry::mark_started` before their first round,
so the heartbeat's immediate first line still covers the startup sessions.
Keep-warm takes the embedder from the template builder
(`MemoryBuilder::shared_embedder`), held before `resolve_role` and dropped on
the proxy branch so a proxy still keeps no model (PR 2 review L1).

**The shutdown's set is taken when the transport ends.** `close_set` sets
`closing`, waits on the attach lock (a background attach in flight finishes
and is in the set), and removes the live sessions from their slots, so stage
6 drops the last registry-held handles where it always did. Stage 3 closes
the set concurrently beside `join_detaches` (a detach in flight finishes its
lease release before exit). `serve` keeps its own `Arc<Memory>` handles until
it returns, as PR 2 decided, so each `Memory`'s drop still runs after the
watchdog is disarmed.

**Serve's usage checks run before the backends (coordinator, for PR 8).**
`main` reads `lambo.toml` once in `serve_preflight` and passes the file to
`resolve_backends`, so a usage error loads no model. A stdio serve with no
`--session` gets clap's own `MissingRequiredArgument` for `--session
<SESSION>` (exit 2, as when the flag was required); more than one is exit 2
too. Both live in `stdio_session`, the one function PR 8 replaces with its
resolver. The notice's keys are per transport, each its own entry: stdio
lists `default_session` and `[[serve.projects]]`, which PR 8 drops.

## Per-session state the earlier PRs asked to keep per session

#14's query cache, #23's erase fencing and the lease fence live inside each
`Memory`, and #18's `TieredStore` keys its per-session state by session; a
registry session is a full `Memory` built by the unchanged `build_attach`, so
each keeps its own. The isolation test drives recall, inspect, saints, stats
and a forced GC through two sessions on one store over HTTP.

## Tests

Existing tests pass unmodified (`ServeOptions` literals use `..new()`).
Added: `mcp::serve::tests::registry` (two sessions served concurrently and
isolated over the real router with a minimal streamable-HTTP client; the
`/mcp` alias and the uniform 404 for unknown, malformed and percent-encoded
ids; the process-wide cap; the policy derivation; `DetachSession` lease loss
keeps the other session serving; closing the registry releases every lease
with its token kept; sixteen dirty SQLite sessions close inside
`SHUTDOWN_BUDGET`; a pinned session held elsewhere is re-elected in the
background; the pinned plan and `check_pinned`),
`tests/serve_multi_session.rs` (a spawned two-session HTTP hub: routing, a
CLI writer refused on both sessions, a second hub answering 503 for both, a
stdio serve proxying into session b, SIGTERM leaving both rows
`lambo:released` with tokens kept), and a `[serve]` notice-keys unit test.
Mutation checks: routing every request to the default fails the routing,
alias and lease-loss tests; no lease watcher fails the lease-loss test.

## For PR 5 to 8

- PR 5: authorize in the router, between `parse_addressed` and
  `registry.lookup` (`transport::addressed_session` and `default_session`),
  with no store call; `warn_if_unenforced` drops `[[serve.credential]]`.
  **Requirement (review L6):** the order is load-bearing. In PR 4 a hosted
  session that is not `Live` answers 503 (`HeldElsewhere`, `Detaching`,
  `Failed`, or the hosted-but-in-no-slot arm) and an unhosted one the
  uniform 404, so anyone past the bearer can tell which names a serve
  hosts. That is acceptable in PR 4 only because one token reaches every
  pinned session (the docs say so). PR 5 must authorize before
  `registry.lookup`, so an unauthorized caller gets the byte-identical 404
  for every slot state, and pin it with a byte-level test (PR 1's
  `on_the_wire`) that an out-of-scope caller gets the same 404 for a
  `HeldElsewhere` session and for a `Detaching` one as for an unhosted
  name.
- PR 6: `Slot` gains `Attaching` (single-flight) for on-demand sessions;
  `lookup`'s "hosted but in no slot" arm becomes the on-demand attach;
  eviction and idle detach reuse `detach` (it is reason-agnostic today and
  always ends in `HeldElsewhere`, which an on-demand detach must replace
  with removing the slot). `attach_lock` serializes background attaches;
  `attach_concurrency` replaces it with a semaphore.
- PR 7: `Slot::Erased` and `Erasing`; an erased pinned session should then
  be an `Erased` slot rather than a startup error.
- PR 8: stdio selection lives in `pin_sessions`' `Transport::Stdio` arm.
