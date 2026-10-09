# #32 PR 2: `serve()` split into a process part and a session part (decisions)

Base: main `db709d3` (PR 1 merged). Design of record: the approved #32 design,
section 8's PR 2 row, with sections 3 and 4. Decisions and why; the commits
carry the mechanics.

PR 2 changes no behaviour. A `lambo serve` logs the same lines in the same
order, exits with the same status on every path, and every existing test
passes unmodified. What changes is where things live, so PR 4 can build many
session parts in one process.

## Module map

| module | part | holds |
|---|---|---|
| `mcp/serve.rs` | process | composition root: pre-lease group, election, proxy branch, arming, the startup calls, the transport, the stage sequence |
| `mcp/serve/process.rs` (new) | process | `ProcessTasks` (renamed from `HolderTasks`): `spawn` (ledger heartbeat, #13 keep-warm, J4 refusal poller), `stop_before_close` (stage 2), `stop` (stage 5) |
| `mcp/serve/session.rs` (new) | session | `AttachedSession` (`mem`, `server`, `hub`, `endpoint`, `tasks`), `SessionTasks` (`event_pump`), `attach`, `closing`, `release_endpoint`, `session_server`, `spawn_event_pump` |
| `mcp/serve/shutdown.rs` | both | `run_and_close_sessions` (stages 1 to 4 over the attached set), `SessionClose`, `join_all`; `run_and_close` kept as the one-session form |
| `mcp/serve/stages.rs` | both | `ShutdownProgress::for_session` (a detach's record: `session` field on every line, no watchdog) |

Everything else in `serve/` is untouched apart from doc references.

Process part, as the design's §3.3 table has it: the embedder and store pool
(in the backends), the listener and transports, the HTTP guards, the signals
and `EarlyShutdown`, the watchdog, the ledger file, `ProcessTasks`. Session
part: the `Memory` (with its lease, lease heartbeat and fence), the
`LamboServer`, the endpoint `Hub` and its address, `SessionTasks`.

## Decisions

**`ProcessTasks` lives in `serve/process.rs`, not `shutdown.rs`.** The design
(§3.1) says "new in `serve/shutdown.rs`, split from `HolderTasks`". `HolderTasks`
already held exactly the process-wide set (keep-warm, heartbeat, refusal
poller), so it is renamed, and moved beside its new `spawn` so the tasks'
start and stop sit in one file. `process.rs` then mirrors `session.rs`.
`shutdown.rs` keeps the budgets, the shutdown future and the close.

**The startup order is kept, so the startup is three calls, not one.** Before
the split the holder startup ran: `LamboServer`, heartbeat, keep-warm, refusal
poller, endpoint bind, event pump, `session attached`. The heartbeat reads the
server, and the endpoint needs it too, so a single "attach the session" call
would move the bind or the pump before the process tasks and reorder the log
lines. `serve()` calls `session_server`, then `ProcessTasks::spawn`, then
`AttachedSession::attach`, which is the old order. The arming argument in
`serve()`'s comments still holds word for word: every one of those steps runs
below `holder_shutdown`.

**The per-session stages run over a set, concurrently.** Stage 3 closes every
attached session through a local `join_all` and stage 6 releases every
endpoint the same way, so one `CLOSE_GRACE` and one `ENDPOINT_RELEASE_GRACE`
cover any number of sessions (design §3.5). Stage 4 aborts every event pump.
`serve()` builds a set of one. `join_all` is twenty lines in `shutdown.rs`
because the crate has no `futures` dependency and PR 2 adds none; it polls on
the calling task (nothing spawned), so nothing must be `'static` and every
line the closes log still reaches the caller's subscriber and span, which the
stage tests' `capture_logs` relies on.

**Outcome over a set.** The transport's error wins (the closes still run, as
before). Otherwise each session's close outcome is logged in set order after
stage 4 (`session closed, tail durable` or `final flush failed ...`) and the
first error is returned. For one session that is the old `match` exactly.

**`run_and_close` stays, as the tests' seam.** Its signature is unchanged and
it is `run_and_close_sessions` over a set of one, which is what `serve()` runs.
`serve()` calls the set form directly, so `run_and_close` is gated to the
builds of its readers (`all(test, store-memory, embed-fixture)`: `serve`'s
close and stage tests and `memory::tests::shutdown`), like
`close_bounded_until` already was. The "close always runs" guarantee is in
`run_and_close_sessions`, which both paths run.

**No `session` field on the single-session lines.** The design allows "stage
lines unchanged except the optional `session`"; the PR's own bar is
byte-for-byte, so the process record logs no `session`.
`ShutdownProgress::for_session` is the record that does, for PR 4's detach. It
has no production caller yet; its `expect(dead_code)` names PR 4 as the first.

**`serve()` keeps its own `Arc<Memory>`.** The attached session holds one and is
taken apart at stage 6. If the shutdown future had already resolved (the
signal path), that would have made stage 6 drop the last handle, running
`Memory`'s drop (the leak guard and its lost-tail warning) inside the watched
shutdown rather than after `serve` returns. Keeping `serve`'s handle leaves the
drop point where it was. Caught in self-review; its own commit.

**The transport serves a clone of the session's server.** The session keeps its
`LamboServer` (PR 4's router needs one per session). A clone shares the
`Arc<Memory>`, the ledger and `started_at`, so the heartbeat's uptime and the
ledger are as before.

## Evidence that the moves are moves

- `bodycmp` (from #27/#28) over every item in `serve.rs` and `serve/*.rs`,
  base `db709d3` against the head: 131 identical. With the declared textual
  substitutions applied to the base (`HolderTasks` to `ProcessTasks`,
  `run_and_close` to `run_and_close_sessions` in comments and doc links, one
  doc link qualified), 137 identical and 7 differing, all intended:
  `serve`, `run_and_close` (now the wrapper), `ShutdownProgress` and its
  `begin` / `end` / `complete` (the `session` field), and `ProcessTasks` (the
  struct doc says why it is process-wide).
- The three blocks extracted from `serve()` (server, process tasks, event
  pump; 75 lines) are identical line for line at `cc0e7a0` apart from the
  parameter spellings and tail expressions the script declares. Two comment
  lines in them changed later, in the structural and docs commits.

## Tests

Existing tests unmodified. Added, in `serve::tests::session_set`: `join_all`
returns outputs in input order; runs the set concurrently (fails on a serial
join, checked by mutation; the first version passed against a serial join
because a bare `sleep` fixes its deadline at creation, so each wait now starts
on its first poll); an empty set; the set-wide close closes both members,
aborts both pumps, logs stages 3 and 4 once and two `session closed` lines;
`for_session` lines carry `session=` and the process record's do not.

## For PR 4

- The transport serves `sessions[0].server`. PR 4 replaces that with the
  `/mcp/s/{session}` router and a `StreamableHttpService` per session.
- `ProcessTasks::spawn` still takes the one session's server and `Memory`. The
  heartbeat (one `stats` line per attached session) and the refusal poller
  (one task over attached sessions, §3.6) must iterate the registry.
- The lease-loss watcher (`DetachSession` policy, §4.2) belongs in
  `SessionTasks`, stopped at stage 5. With `ExitProcess` (one pinned session),
  `wind_down`'s fence arm stays the exit path, unchanged.
- A detach runs stages 1, 3, 4, 5 and 6 for one session under
  `ShutdownProgress::for_session`, reusing `run_and_close_sessions`' close
  and `AttachedSession::release_endpoint`; stage 1 is the per-session MCP
  close, not the transport drain.
- `serve()` keeps its own `Arc<Memory>` today only to hold the drop point;
  with many sessions, each detached session's `Memory` drops at its detach,
  which is outside any watchdog by design (§3.4).
