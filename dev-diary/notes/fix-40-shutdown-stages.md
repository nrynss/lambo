# #40: shutdown stage logging and the shutdown watchdog (findings)

Branch `fix/40-shutdown-stages`, from main `3df6812` (#28 merged). The live
stall is **not reproduced**; the root cause is narrowed, not proven. What
changed, why, what was ruled out, and what the next occurrence will capture.

## What the live evidence says

From the issue (v0.3.0, pid 67104, 2026-10-07): `16:27:57.464Z lambo serve:
shutdown signal received, winding down`, then nothing; no `session closed`,
no `call ledger closed`, no grace-window warning; the process gone by 16:29:33;
the next start removed a stale endpoint.

Every shutdown wait after that line is bounded by a tokio timer, and every
one of those timers **logs when it fires**:

| stage | bound | line a firing bound prints |
|---|---|---|
| 1 transport drain | `SHUTDOWN_GRACE` 5 s | `mcp http: connections still open after the grace window ...` |
| 3 session close | `CLOSE_FLUSH_GRACE` 8 s | `close() did not finish within the grace window ...` |
| 3 abandoned-close lease release | `LEASE_RELEASE_GRACE` 2 s | `could not release the single-writer lease within its window ...` |
| 3 clean close | | `lambo serve: session closed, tail durable` |
| 6 endpoint release | `ENDPOINT_RELEASE_GRACE` 3 s | `endpoint sessions did not end within the release grace ...` |
| 7 ledger close | `SHUTDOWN_DRAIN` 0.5 s (wall clock, `thread::sleep`) | `lambo serve: call ledger closed` |

Silence for longer than 13 s after the signal (stages 1 and 3 together) means
those timers did not fire. Two things do that, and nothing else does:

1. **A synchronous block inside `serve`'s own future.** `main` drives `serve`
   with `Runtime::block_on`; the transport timeout and the close timeout are
   polled by that same thread, so a poll that blocks (a `parking_lot` lock
   that never comes free, a blocking `Drop`) means the timer is never looked
   at.
2. **Every runtime worker blocked at once.** A multi-thread runtime's timers
   are driven by a worker that parks; a `block_on` thread never drives them.
   `serve::tests::watchdog::an_async_shutdown_bound_does_not_fire_on_a_wedged_runtime`
   pins it: with the only worker blocked, a 50 ms grace has not fired a
   second later.

The runtime was alive when the signal arrived: the `winding down` line is
printed after `wind_down` resolves, and tokio delivers signals through its
own driver. So whatever wedged it happened between the signal and the 5 s
stage-1 bound, on the shutdown path or concurrently with it.

Two readings of the issue's evidence, corrected:

- "the lease was still being refreshed (expiry 16:28:33)": with
  `LEASE_TTL` 45 s and `LEASE_HEARTBEAT_INTERVAL` 15 s, that expiry is a
  refresh at 16:27:48, nine seconds **before** the signal, and the next one
  was due at 16:28:03. `re-embed` ran at about 16:27:58, so it cannot tell
  whether refreshes continued. It is consistent with a wedged process and
  with one already dead.
- "launchd killed it": inferred from the plist's missing `ExitTimeOut`. No
  crash report (`.ips`) exists for any lambo process in
  `~/Library/Logs/DiagnosticReports` or `/Library/Logs/DiagnosticReports`
  (checked 2026-10-08; only `.diag` resource reports, none for pid 67104), so
  it did not crash; a SIGKILL leaves no report. The unified log no longer
  holds launchd's lines for that window.

## What was audited and ruled out

Every await from the signal through stage 3, plus stages 6 and 7:

- **Transport drain**: `run_until_shutdown` cancels, then
  `timeout(SHUTDOWN_GRACE, running)`. An open SSE stream ends by that
  timeout, as the issue's scratch runs showed (5.1 s). Bounded.
- **rmcp service cancellation**: inside the stage-1 timeout; the endpoint's
  sessions inside stage 6's `ENDPOINT_RELEASE_GRACE`. Bounded.
- **Keep-warm**: aborted synchronously in stage 2. Its embed waits on the
  candle coalescer's oneshot; the forward itself runs on the coalescer's own
  OS thread, never on a tokio worker, and `CandleEmbedder`'s drop does not
  join that thread. Not a blocker.
- **`Memory::close`**: every await (`close_state`, `stop_replay`'s join, the
  quiesce, the writers gate, the producer and flush joins, the final flush,
  the lease release) is inside `close_bounded`'s `CLOSE_FLUSH_GRACE`. The
  writers gate can wait on an embedder call that never returns (the L1 note
  from #27), and the worker joins can wait on a task stuck in a synchronous
  stretch, but each of those is an *async* wait the 8 s timeout ends. They
  cannot produce silence on their own.
- **The quiesce**: bounded by `WRITE_QUEUE_DRAIN_BUDGET` (2 s); the join that
  follows has no timer of its own (now logged) but sits inside the close's.
- **Embedder HTTP calls** (`embed-bge`, `embed-gemini`): async reqwest, and
  every gated write is under `HYBRID_IO_TIMEOUT`. The live rig used candle.
- **Lock order**: graph → scores → index → hot list in recall
  (`Daemon::recall_with`), graph → hot in the daemon cycle, graph → index in
  GC and `mirror_concepts`. Consistent; no inversion found. `parking_lot`'s
  `RwLock` deadlocks on a recursive read while a writer waits; no recursive
  graph read was found on the paths read (recall, stats, saints, daemon
  cycle), but the audit was by reading, not exhaustive.
- **Blocking calls on runtime threads**: none in production paths except the
  ledger's bounded `thread::sleep` drain in stage 7 and `store::load`'s
  startup-only private-thread `block_on`.
- **The re-embed path**: correctly refused by the lease at 16:27:58; not a
  shutdown participant.

So no unbounded *async* wait exists on the shutdown path; the stall needs a
synchronous wedge, and reading did not find one.

## Reproduction attempts (scratch store, port 17740, private runtime dir)

Release build of this branch (`ship,embed-candle-metal`), candle on Metal,
SQLite, HTTP, ledger with heartbeat, endpoint bound. All exited cleanly, with
every stage logged:

| shape at SIGTERM | exit | where the time went |
|---|---|---|
| idle | 0.01 s | |
| 3 clients, 2 GET SSE streams, 6 threads of derives (1 to 30 concepts, up to 1,500 words) and recalls, a ~220-job queue backlog, coalescer saturated | 7.1 s | stage 1 5.0 s (SSE grace), stage 3 2.0 s (quiesce deferred 222 writes) |
| 1.5 s after start, 227 durable intents replaying, burst starting | 7.2 s | stage 1 5.0 s, stage 3 2.0 s (149 deferred) |

The second start also reproduced the issue's other oddity: **`the embedder
could not be probed within 5s`** appears when the startup replay of durable
intents keeps the coalescer busy, so the calibration probe waits behind it.
The live 0.3.0 first start likely had intents owed by its predecessor. That
is telemetry only (`write_queue_measured=false`), not a fault, and unrelated
to the stall.

What the live case had that none of these did: launchd as parent with no
`ProcessType` (light resource limits), real Claude Code and Codex clients,
3,688 concepts, the first start after a binary swap.

Side observations (not #40, recorded for their owners): on this scratch store
the write queue served about 0.15 serial items/s with workers parked in
`hybrid::checked_candidates` on the single SQLite worker's vector query (#8's
graph-backed vector source addresses that path); and a burst can push hybrid
embeds past `HYBRID_IO_TIMEOUT` (30 s) when the coalescer is saturated.

## What changed

1. **Stage logging** (`feat(obs)`): see *Log lines* below.
2. **Watchdog** (`fix(serve)`): an OS thread, started by the first shutdown
   stage, independent of the runtime. It warns once per stage that runs past
   `Stage::bound()` + 1 s (only a stage whose timer did not fire can), and at
   `SHUTDOWN_WATCHDOG` (20 s) logs the stage at ERROR and calls
   `process::abort`. Abort, not exit: SIGABRT gives a macOS crash report with
   every thread's stack, the capture the issue's runbook asked for by hand.
   Its log lines go through a helper thread waited on for at most 1 s, so a
   blocked stderr cannot hold the abort. `serve` disarms it at the end and
   through a drop guard on every other way out.
3. **Docs**: `SHUTDOWN_BUDGET`'s doc no longer calls itself the time to exit;
   `close_bounded_until`'s doc no longer says the pre-arm is not wired in.

Durability is unchanged. By 20 s every async bound has expired, so a close
that had not finished would already have been abandoned and its tail lost;
the lease lapses at its TTL exactly as after launchd's SIGKILL. Stage order,
abort order and the close's custody rules are untouched.

## Bounds

| constant | value | what it bounds |
|---|---|---|
| `SHUTDOWN_GRACE` | 5 s | stage 1 |
| `CLOSE_GRACE` (`CLOSE_FLUSH_GRACE` 8 + `LEASE_RELEASE_GRACE` 2) | 10 s | stage 3 |
| `SHUTDOWN_BUDGET` | 15 s | stages 1 to 4: signal to durable tail and released lease |
| `hub::ENDPOINT_RELEASE_GRACE` | 3 s | stage 6 |
| `ledger::SHUTDOWN_DRAIN` | 0.5 s | stage 7 |
| `watchdog::EXIT_BUDGET` | 18.5 s | the whole healthy shutdown |
| `watchdog::SHUTDOWN_WATCHDOG` | 20 s | the hard stop, timers or not |
| `LEASE_TTL` | 45 s | > `SHUTDOWN_WATCHDOG` |

`EXIT_BUDGET < SHUTDOWN_WATCHDOG < LEASE_TTL` and the existing relations are
build-time asserts. **Supervisor**: the kill timeout must exceed 20 s for the
watchdog to act first; 30 s recommended. launchd's default `ExitTimeOut` is
20 s, which races it. Recommended plist change for the dogfood unit (not
applied; the operator does it with bootout, edit, bootstrap):

```xml
  <key>ExitTimeOut</key><integer>30</integer>
```

`DOGFOOD-SETUP.md` carries it in the unit, the stalled-stop runbook, and the
§2a wait-for-the-pid step before offline verbs.

## Log lines (stable; grep for these)

Serve stages, at INFO, target `lambo::mcp::serve::stages`, fields `stage`,
`stage_name`, `elapsed_ms`:

```text
lambo serve: shutdown watchdog armed: the process aborts if the shutdown is still running in 20000 ms
lambo serve: shutdown stage 1/7 transport_drain started
lambo serve: shutdown signal received, winding down
lambo serve: shutdown stage 1/7 transport_drain finished in 5005 ms
lambo serve: shutdown stage 2/7 keep_warm_abort started|finished in N ms
lambo serve: shutdown stage 3/7 session_close started|finished in N ms
lambo serve: shutdown stage 4/7 event_pump_abort ...
lambo serve: shutdown stage 5/7 background_tasks ...
lambo serve: shutdown stage 6/7 endpoint_release ...
lambo serve: shutdown stage 7/7 ledger_close ...
lambo serve: shutdown finished in N ms
```

`Memory::close` steps inside stage 3, at INFO, target `lambo::memory::shutdown`,
fields `session`, `step`, `step_name`, `elapsed_ms`:

```text
close: step 1/10 serialize started|finished in N ms
close: step 2/10 replay_stop ...
close: step 3/10 queue_quiesce ...
  write queue: quiesce wait ended after N ms (K job(s) still outstanding)
  write queue: W worker(s) aborted and joined in N ms
close: step 4/10 writers_gate ...
close: step 5/10 heartbeat_abort ...
close: step 6/10 producer_joins ...
close: step 7/10 flush_join ...
close: step 8/10 final_drain ...
close: step 9/10 final_flush ...            (or: final_flush skipped (empty tail))
close: step 10/10 lease_release ...
close: step N/10 <name> abandoned after M ms (WARN: the close future was dropped mid-step)
```

Watchdog, from its own thread:

```text
WARN  lambo serve: shutdown watchdog: stage N/7 <name> has run M ms, past its B ms bound; ...
ERROR lambo serve: shutdown watchdog: shutdown still running after M ms, in stage N/7 <name>; aborting ...
```

The names are pinned by `serve::tests::stages::the_stage_names_and_numbers_are_stable`
and the watchdog lines by `serve::tests::watchdog::the_watchdog_lines_name_the_stage`.

## What the next occurrence will show

- The last `started` without a `finished` names the stage, and inside stage 3
  the close step.
- A watchdog WARN for that stage says its own timer did not fire, which
  confirms a wedge rather than a slow dependency.
- At 20 s the ERROR line and an `.ips` crash report with every thread's stack:
  which thread is blocked, on what. That is the root cause the issue needs.

The watchdog cannot help if the runtime is wedged *before* the signal is
delivered (no `winding down` line at all), because signal delivery itself
runs on the runtime. That is not the shape #40 showed.

## For #31, #32 and #13

- #31 (defer the embedder load until the role is known) is untouched; the
  probe-timeout finding above is relevant to its startup measurements.
- #32 (multi-session serving): `ShutdownProgress` is per serve process; a
  per-session detach would take a progress of its own for stages 1 to 5 and
  share 6 and 7. The watchdog is process-wide by design.
- #13: the keep-warm abort is stage 2 and is logged; nothing about it changed.
