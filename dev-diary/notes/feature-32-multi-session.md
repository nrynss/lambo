# #32 multi-session serving: the consolidated record

One `lambo serve --transport http` process now holds many sessions, each
still with exactly one writer. This note is the single record of #32. It
covers the design that was approved, every decision made while building it
in PRs 1 to 8, and each place where the shipped code differs from the design,
with the reason. The per-PR notes keep the full arguments and test lists.
This note says what is true on main at `fe8c3d71` and points to them.

- Issue: [#32](https://github.com/nrynss/lambo/issues/32). Design of record:
  [issue comment 6067936723](https://github.com/nrynss/lambo/issues/32#issuecomment-6067936723).
  The repo owner approved it on 2026-10-09, accepting all 19 of its
  recommendations.
- Spec: §2.2 is the single-writer rule. The deviation is recorded in
  [spec-deviation-2-2-many-leases.md](spec-deviation-2-2-many-leases.md).
- Operator docs:
  - [`docs/reference/cli.mdx` § Several sessions in one serve](../../docs/reference/cli.mdx);
  - [`docs/reference/config.mdx` § Multi-session serving](../../docs/reference/config.mdx);
  - the rig runbook, [DOGFOOD-SETUP.md § 7](../lambo-for-mooshik/DOGFOOD-SETUP.md).

## The PRs

| PR | what | merged | merge | note |
|---|---|---|---|---|
| 1 | `surface::session`, `[serve]` parse, ledger `session` field | [#59](https://github.com/nrynss/lambo/pull/59) | `db709d35` | [pr1](feature-32-pr1-session-surface.md) |
| 2 | `serve()` split into process and session parts | [#63](https://github.com/nrynss/lambo/pull/63) | `163d54e7` | [pr2](feature-32-pr2-serve-split.md) |
| 3 | one calibration probe per process per embedder | [#69](https://github.com/nrynss/lambo/pull/69) | `42a5ead8` | [pr3](feature-32-pr3-calibration.md) |
| 4 | `SessionRegistry`, `/mcp/s/{session}`, pinned sessions | [#72](https://github.com/nrynss/lambo/pull/72), flake fix [#77](https://github.com/nrynss/lambo/pull/77) | `22441345`, `7afb2774` | [pr4](feature-32-pr4-registry.md) |
| 5 | credentials and authority | [#80](https://github.com/nrynss/lambo/pull/80) | `5773e4a1` | [pr5](feature-32-pr5-credentials.md) |
| 6 | on-demand sessions within the `[serve]` bounds | [#86](https://github.com/nrynss/lambo/pull/86) | `8e47697d` | [pr6](feature-32-pr6-on-demand.md) |
| 7 | admin surface and in-serve erase | [#91](https://github.com/nrynss/lambo/pull/91) | `fe8c3d71` | [pr7](feature-32-pr7-admin-erase.md) |
| 8 | stdio cwd map, `[[serve.projects]]` | [#75](https://github.com/nrynss/lambo/pull/75) | `2221fe40` | [pr8](feature-32-pr8-cwd-map.md) |
| 9 | this note, the spec deviation, reference docs, rig runbook | branch `docs/32-pr9-docs` | | |

They landed in the order 1, 2, 3, 4, 8, 5, 6, 7. PR 6 merged before PR 7,
and PR 7 was refitted onto PR 6's attach permits and negative cache (PR 7
note, "Merged with PR 6").

One related PR outside #32: [#90](https://github.com/nrynss/lambo/pull/90)
(#4 PR 3, merge `9c63b060`). It lets `serve-web` inherit
`[[serve.credential]]`, and it depends on rules that this work owns (below).

## What the design set out (in brief)

- **Two scopes.** Project scope (agents, the rig) uses a few *pinned*
  sessions. They are named in config, attached at startup, and never
  idle-detached. User scope (apps) uses many *on-demand* sessions. Each is
  attached on its first authorized request, then idle-detached or evicted
  least-recently-used at the cap.
- **Selection.** Over HTTP the session is a path segment,
  `/mcp/s/{session}`, and `/mcp` is an alias for the default session. A stdio
  serve has one session: `--session`, else the `[[serve.projects]]` cwd map,
  else `default_session`.
- **Registry.** Each attached session is the same as the old one-session
  holder: one `Memory`, one `LamboServer`, one endpoint socket, one lease and
  fencing token, and its own rmcp `StreamableHttpService`. The process shares
  the embedder, the store, the listener, the guards, the signals, the
  watchdog, the ledger file and the calibration probe.
- **Authority.** `[[serve.credential]]` entries each have a `token_env`, a
  scope (`sessions` and/or `session_prefix`) and the flags `create`, `erase`
  and `admin`. Authorization happens in memory before any store call. Any
  refusal gets one uniform 404.
- **Lease loss.** A one-session serve keeps `ExitProcess`. Any other serve
  uses `DetachSession`: only the session that lost its lease is detached and
  re-elected, and the other sessions keep serving.
- **Erase.** `POST /admin/s/{s}/erase` runs on the serve that holds the
  session. It erases as the lease's holder, so no other process can take the
  session in between. It is not an MCP tool.

## Decisions as shipped, by area

Each heading names the PR. Where the shipped behavior differs from the
design, a **Deviation** line says so. The table at the end lists every
deviation in one place.

### Addressing and the uniform 404 (PR 1, PR 4)

- `AddressedSessionId` is a separate type from `types::SessionId`. It proves
  the strict charset ran: `[A-Za-z0-9._:-]`, 1 to `MAX_ADDRESSED_LEN` (128)
  bytes, no leading `.`, and no percent-decoding (`surface::session`).
  `--session` keeps its looser rule. A one-session serve can still use a
  loose name, and that session is reached only at `/mcp`.
- The uniform 404 is axum's own unrouted 404, with no headers and an empty
  body. A refused session cannot be told apart from a path the server never
  routed. Two rules keep it that way: every route answers every method
  (`any`), and every guard goes on `.layer`, never `.route_layer`.
- Every session name in `[serve]` must pass the strict charset, not only the
  pinned ones. With more than one pinned session, `check_pinned` also
  applies it to `--session` names.
- `/mcp/<anything else>` is now a 404. Before, it reached the one session,
  because rmcp ignores the path. This is recorded in the changelog under
  Breaking. `/mcp/` with a trailing slash is included.

### The process / session split (PR 2)

- `ProcessTasks` moved to `serve/process.rs`, not `shutdown.rs`, which is
  where the design put it. Its tasks are the heartbeat, keep-warm and the
  refusal poller. Its start and its stop now sit in the same file.
- Startup is three calls (`session_server`, `ProcessTasks::spawn`,
  `AttachedSession::attach`), not one. This keeps the old log-line order
  byte for byte.
- The per-session stages run concurrently over a set, through a local
  `join_all` that isolates panics. One `CLOSE_GRACE` (10 s) and one
  `ENDPOINT_RELEASE_GRACE` (3 s) cover any number of sessions.

### Calibration (PR 3)

- `EmbedderCalibration` is keyed by embedder identity and held through a
  `Weak`, so a proxying serve still drops its model. It is lazy: a serve
  that only proxies never probes.
- **Deviation from "once per process" (decision 14).** A probe that failed
  or was aborted is run again by the next attach, at most once per
  `PROBE_RETRY_BACKOFF` (60 s). A measured probe is never repeated. Without
  the retry, a remote embedder timing out during the first attach would
  have left every later session without probe telemetry for the life of
  the process.

### The registry and pinned sessions (PR 4)

- **Deviation (§2.5).** The design builds a one-session registry for every
  legacy serve. In the shipped code, a one-session serve still goes through
  `resolve_role`: the J2 election and the proxy are unchanged. The holder is
  then wrapped in a registry of one. Two or more pinned sessions take
  `serve_pinned`, which has no election. Both paths end in one
  `close_holder`.
- The lease-loss policy is derived in one function,
  `LeaseLossPolicy::for_scope(pinned, on_demand)`. It returns `ExitProcess`
  only for one pinned session with nothing on demand, and `DetachSession`
  otherwise (design R4).
- A pinned session that another process holds becomes `HeldElsewhere`. It
  answers 503 with `Retry-After`, and a background retry runs every
  `PINNED_RETRY`. That is `ELECTION_RETRY` × 5, so 5 s, and never less than
  1 s. The `lease:refused` ledger line is written once, at the transition.
- **Addition: `Slot::Failed`.** An attach error that will not clear on its
  own gets this slot: an unprovisioned store, or a mismatched embedding
  contract. It answers 503 without `Retry-After` until an operator restarts
  the serve. An unreachable store or embedder is still retried.
- A detach drops the registry's handle and keeps only a `Weak` to it. The
  retry then waits up to `PREVIOUS_HANDLE_WAIT` (30 s) for that handle to
  go, so a fenced handle is released before the session attaches again.
- The default session must be pinned. `/mcp` never attaches on demand
  (PR 4, kept by PR 6). PR 1's review had left open what `/mcp` serves when
  `default_session` is not hosted. The answer is a startup refusal.
- The `max_attached` cap is checked again after the union of `--session`
  and `[serve] sessions`.

### Credentials and authority (PR 5)

- The credential table: nothing configured on loopback gives the implicit
  `local`. Nothing configured beyond loopback refuses the start. The legacy
  token is `default`. Configured entries sit beside `default`. `local` goes
  away as soon as anything is configured. Neither `default` nor `local`
  carries `create`, `erase` or `admin`, so erase over the wire always needs
  a configured credential, on loopback too (Q5).
- `default` and `local` reach "every pinned session". This is a fourth kind
  of scope (`SessionScope::pinned`), narrower than `"*"`. A `"*"` also
  covers every name under any credential's prefix.
- `surface::bearer::match_any` scans every credential in constant time,
  with no early exit. A missing header is scanned as an empty token. A
  presented token over 4 KiB is refused before the scan, and two
  `Authorization` headers get 401.
- The order is fixed: bearer check (401), then the id's shape, then the
  scope, all in memory, then `registry.lookup`. An out-of-scope caller gets
  the byte-identical 404 for a session in any state, and the store is not
  called. A 503 is visible only inside scope.
- **Deviation: fair shares with a floor (§3.6).** The design kept the
  global `RateLimiter` and a process-wide `max_sessions`, with no numbers
  per credential. The shipped rules:
  - `--rate-limit-rps` is per credential (`CredentialRates`, burst 2×), so
    the process-wide bound is the rate × the number of credentials;
  - each credential may hold `credential_share = max(1, floor(max_sessions
    / credentials))` MCP sessions, checked after the process cap.

  Why the floor: the shares never add up to more than the cap, so every
  credential, the operator's included, can always open its share. Unused
  shares are not lent to other credentials. With one credential the share is
  the whole cap and there is one bucket at the old rate, so the rig is
  unchanged.
- An MCP session belongs to the credential whose `initialize` opened it.
  The opener is recorded when rmcp mints the id (`AttributingSessions`).
  Another credential that presents the id gets rmcp's own unknown-id answer.
- Only a request that would mint an MCP session (an `initialize`) has its
  body buffered before the cap reserves a slot. The body must arrive within
  `REQUEST_BODY_TIMEOUT` (30 s), or the answer is 408. A body still
  arriving holds no slot.
- The Host check: rmcp's loopback allow-list stays only for the implicit
  `local` credential. Any serve that requires a bearer accepts any `Host`.
- `call` ledger lines made through a configured credential carry an
  additive `credential` field.

### On-demand sessions (PR 6)

- **When it is on.** It is on exactly when the serve is HTTP and some
  configured credential reaches past the pinned sessions, through a
  `session_prefix` or an unpinned exact name
  (`authority::reaches_past_pinned`). Such a serve runs as a registry under
  `DetachSession`, even with one pinned session. The legacy `default` and
  `local` credentials never turn it on, so the rig keeps `ExitProcess`.
- **The attach runs only after authorization.** Out of scope there is still
  no store call.
- **Deviation: probe before evict (§3.2 order).** The design checked
  capacity and evicted (step 3) before the attach (steps 4 and 5). PR 6's
  first cut did the same. The review (H1) found that any request in scope
  could then evict a live session, including one for an absent, erased or
  held id. Shipped order:
  1. `route_or_start` inserts `Attaching { placed: false }` and evicts
     nothing;
  2. the attach task takes a permit and probes the lease row
     (`PROBE_TIMEOUT`, 5 s);
  3. an erased row, an absent row without `create`, or a row held by
     another live writer ends the attach there;
  4. only then does `reserve_place` check capacity under the slots lock
     and pick a victim.

  So nothing is ever evicted for a request that will not attach.
- **Single flight.** `Slot::Attaching` holds a `watch::Receiver`. N
  concurrent first requests cause one `build_attach`. The attach runs on a
  spawned task that the registry tracks. A waiter whose credential has
  `create` ignores an `Absent` outcome from a flight that had no `create`. A
  request follows a session through at most `ATTACH_FOLLOWS` (3) attaches
  before it answers 503.
- **Permits.** `attach_concurrency` is a semaphore (always 1 on SQLite,
  never 0). It replaced PR 4's `attach_lock` for on-demand attaches and for
  the pinned retry alike. `close_set` takes every permit.
- **Timeouts.** These are not in the design (review M2):
  - `ATTACH_TIMEOUT` 60 s for the whole attach, which also bounds the
    pinned retry's acquire;
  - `ATTACH_WAIT` 15 s for each waiting request;
  - `ATTACH_BUSY_RETRY` 5 s as the `Retry-After` when either one fires.

  A timed-out attach releases its lease and clears its slot.
- **Deviation: the negative cache (§3.1).** The design had a `Slot::Erased`
  negative cache with a bounded LRU. The shipped `NegativeCache`
  (`registry/on_demand.rs`) remembers `Absent`, `Erased` and `Failed`.
  - `Absent` is recorded only from a flight without `create`, and a
    request with `create` ignores it.
  - Entries live for `NEGATIVE_TTL` (30 s), at most `NEGATIVE_CACHE_MAX`
    (1024) of them, and the oldest goes first. A cached entry takes no
    place.
  - Held-elsewhere and transient errors are never cached.

  Why: every repeat of an absent or failed id would otherwise wait in line
  for the single SQLite permit (review M3).
- **Eviction.**
  - Only a `Live` on-demand session with no tool call in flight can be
    evicted, and only after `EVICT_MIN_IDLE` (2 s) unused. That floor
    covers the gap between rmcp's POST handler answering and the call
    entering `call_tool`'s `InFlight` guard. The router holds the
    session's `InFlight` from routing until the request's handler returns.
  - With none to evict, the answer is 503 with `AT_CAPACITY_RETRY` (5 s).
    The design gave no number.
  - Pinned sessions are never candidates.
- **Fair place shares.** The on-demand places are `max_attached - pinned`.
  Each credential that reaches on-demand sessions has the share
  `place_share(places, credentials)`, the same floor rule as above, at least
  1. The share limits eviction only: a free place goes to whoever asks. At
  the cap, a credential at or over its share may evict only its own
  least-recently-used idle session. One under its share may also evict from
  a credential over its share. A session counts against the credential
  whose request attached it (the `owners` map).
- **Unplaced cap.** An attach that has not probed yet takes no place. Once
  `UNPLACED_PER_PLACE` × `max_attached` (that is, 2 × `max_attached`) such
  attaches are waiting, a new one answers 503 with `Retry-After: 5` and
  starts nothing.
- **Idle detach.**
  - Idle is measured by tool calls, not connections, so an open SSE stream
    does not count as use (R7).
  - The sweeper runs every `min(30 s, idle_detach)`.
  - `idle_detach` is never below `MIN_IDLE_DETACH` (1 s), and `serve`
    refuses a value under 1 s.
- **Held elsewhere, on demand.** The answer is 503 with `Retry-After` set to
  when the holder's lease could lapse (at least 1 s). The slot is removed:
  there is no background retry and no `lease:refused` line.
- **Detach ends by scope.** A pinned session goes to `HeldElsewhere`. An
  on-demand session's slot is removed, and its old handle is kept weakly in
  `previous` until it drops. The close flushes and releases the lease, and
  the token is kept (#23). A reattach therefore mints `token + 1` and
  recalls what the session held.
- **Per-session rate.** Each session has its own bucket at
  `per_session_rps`, drawn after the credential's bucket and after the
  lookup. It answers 429 with `Retry-After: 1`.

  **Deviation (§3.6).** The design defaulted the bucket to
  `--rate-limit-rps` everywhere. A one-session serve that attaches nothing
  on demand draws a per-session bucket only when `per_session_rps` is set
  (`unwrap_or(0)`). It therefore answers exactly as before (review L3).
- Refusal cursors are kept while a session is pinned or attached, and for
  `LEASE_TTL` (45 s) after a detach, at most 1024 of them. A reattach
  therefore does not record its refusals again.

### Admin surface and in-serve erase (PR 7)

- **Authorization comes before anything else.** The order is shape, then
  scope, then the `erase` capability, all in memory, and the body is not
  read before that. Any refusal is the uniform 404. Inside scope only, the
  route answers 405, 400 (bad body, a confirm mismatch, or more than
  `MAX_ADMIN_BODY_BYTES`, 1 KiB), 408, 409, 500 and 503. `/admin/` requests
  are never MCP-session openers, so an erase is never refused at the
  MCP-session cap.
- **Deviation: fence first, then erase as the holder (§6.3 step 2).** The
  design ends the MCP sessions, erases as the holder, then fences and tears
  down. The shipped order:
  1. set the slot to `Erasing`;
  2. stop the lease watcher;
  3. `Memory::fence_for_erase` latches the fence in-process, with the
     tombstone as the winner;
  4. end the MCP sessions;
  5. `close_bounded` on the fenced handle: the pipeline is quiesced, the
     writers gate drained, and every task aborted *and joined*; the tail
     is discarded and the lease is **not** released;
  6. abort the event pump and release the endpoint;
  7. `store.erase_session(id, mem.lease_holder())`;
  8. set the slot to `Erased`.

  Why: neither order releases the lease, so both keep Q7's no-gap property.
  Fencing first makes "nothing of this session runs during the erase" true
  by construction. That includes a flush that has committed and would
  otherwise mirror into the #18 recall index after the tiered erase swept
  it. The store fence alone does not cover that case.
  - The Opus review endorsed this. The heartbeat stops at the close, at
    least 30 s of the 45 s TTL remain, and the steps before the erase take
    about 13 s at worst.
  - If the lease lapses anyway and another writer takes the session, the
    erase answers 409.
  - The cost: an erase that fails before its commit has already discarded
    the RAM tail.
- If `close_bounded` is abandoned (the writers gate stays held past its
  bound, or a second signal arrives), the erase still aborts and joins every
  task first (`Memory::stop_tasks_for_erase`; review M1).
- **Deviation: `ERASE_PERMIT_WAIT`.** `erase_now` takes every attach permit
  (`acquire_many`) *before* it claims the slot, then drops them right after
  the claim. From then on, the `Erasing` slot keeps every attach of the id
  away.
  - Why permits before the claim: if the erase claimed first, a pinned
    retry could admit a live session over `Erasing` and orphan a `Memory`
    (review H1).
  - Why the permits are not held through the store erase: that would stall
    every other attach and the shutdown's `close_set` behind the store call.
  - The acquire is bounded by `ERASE_PERMIT_WAIT` (10 s). An attach in
    flight can hold a permit for up to `ATTACH_TIMEOUT` (60 s), and the
    semaphore is FIFO. On timeout the erase answers 503 with
    `Retry-After: 1` and has claimed nothing.
- Every final slot write in the erase and the retry is conditional on the
  slot still being in the state that write expects (`admit_if`,
  `insert_live_if`, `replace_held`). An admission that is refused closes
  whatever it built.
- **Deviation: the -32003 vs 410 split (§6.2).** The design said an erased
  session answers MCP error `-32003`, or HTTP 410 on admin routes. Shipped:
  - a POST carrying exactly one JSON-RPC request gets 200 with the proxy's
    own erased frame (`proxy::erased_reply`, code `-32003`);
  - a GET, a DELETE, a notification, a response, a batch, or an unreadable
    body gets 410, because none of them has a request id to answer;
  - a tool call already inside the session when it was fenced gets the
    #23 erased error from the tool layer.

  The erased answer never reaches rmcp, which is what makes the JSON-RPC
  frame possible.
- `Erasing` answers 503 with `Retry-After: 1`; 410 is only for `Erased`
  (review L3). `Erasing` takes a place, because the handle holds memory
  until its close. It is never evicted and never idle-detached.
- An erased pinned session keeps `Slot::Erased`. An erased on-demand id
  drops its slot and its owner, and goes into the `NegativeCache` as
  `Erased`. There is no separate erased cache. After that entry expires,
  the probe answers from the tombstone.
- At startup and on a retry, an erased pinned session is decided by the
  lease row (`read_tombstone`), never by error text. It becomes `Erased`
  and the other sessions start (before PR 7 the serve refused to start). A
  one-session serve still refuses to start on an erased session.
- A one-session serve (`ExitProcess`) exits after it erases its own
  session. The erase's fence is announced on every way out of
  `erase_attached` (`AnnounceOnDrop`, holding a `Weak<Memory>`), and only
  after the answer has been handed off. So the wind-down starts, and the
  transport drains, only once the 200 has gone out.
- The erase runs on a task that the registry tracks
  (`track_unless_closing`). A client that hangs up cannot cancel it. The
  shutdown waits for it for up to `ERASE_SHUTDOWN_GRACE`, which equals
  `CLOSE_FLUSH_GRACE` (8 s), and then cuts it short with an unknown
  outcome (500).
- The fenced close now joins what it aborts. This applies to every fenced
  close, not only an erase: a close after a lost lease had the same mirror
  window.
- **Deviation: `POST /admin/s/{s}/detach` is not served (§6.3).** A pinned
  session's detach would be undone by the background retry within 5 s. An
  operator detach needs its own semantics first. It is left to #33 or a
  follow-up.
- **Deviation: `GET /admin/sessions` has no `last_used` field (§6.3).** The
  fields are `session`, `state`, `pinned` and `default`. For a live session
  there is also `attached`: `nodes`, `edges`, `concepts`,
  `embedded_concepts`, `estimated_vector_bytes` (embedded concepts × dim ×
  4) and `log_depth`. The states are `attaching`, `live`, `detaching`,
  `held_elsewhere`, `failed`, `erasing`, `erased` and `unattached`. The last
  is a pinned id that is in no slot, between states. The listing runs on
  the blocking pool.

### The stdio cwd map (PR 8)

- `--session` short-circuits everything: neither the cwd nor the map is
  read. Otherwise the longest `[[serve.projects]]` entry wins, compared by
  whole path components on canonical paths (symlinks are followed on both
  sides). After that comes `default_session`. Otherwise the serve refuses
  with clap's own missing-argument error, exit 2.
- An entry whose path does not exist is skipped. A cwd that cannot be
  resolved falls back to `default_session`, with a WARN. Two entries for
  one directory that name different sessions are refused, exit 1. With
  `HOME` unset or relative, `~` entries are skipped with one WARN.
- Nothing quotes the cwd or `$HOME`. A global map needs `--config` or
  `LAMBO_CONFIG`.
- An HTTP serve never reads the map. The startup notice (below) names it
  when an HTTP serve's table sets it.

## Retry-After, every value

| answer | `Retry-After` | where |
|---|---|---|
| guard rate limit (credential bucket), per-session bucket | `1` | `http_guards.rs`, `transport.rs` |
| MCP-session cap, or the credential's share of it | `5` | `http_guards.rs` |
| `Detaching`, `Erasing`, a pinned id between states | `1` | `registry.rs` (`answer_for`, `lookup`) |
| pinned `HeldElsewhere` | the time to the next retry, at least 1 | `registry.rs` |
| `Failed` | none | `transport.rs` |
| on-demand held by another writer | when that lease could lapse, at least 1 | `registry.rs` |
| no place to evict (`AT_CAPACITY_RETRY`) | `5` | `registry.rs` |
| attach wait or attach timeout (`ATTACH_BUSY_RETRY`), unplaced cap | `5` | `registry/on_demand.rs` |
| erase: `Erasing`, `Attaching`, `Detaching`, permits not in time, shutting down | `1` | `registry/erase.rs` |

## Constants

| constant | value | file |
|---|---|---|
| `DEFAULT_MAX_ATTACHED` | 16 | `src/config/serve.rs` |
| `DEFAULT_ATTACH_CONCURRENCY` | 2 (1 on SQLite) | `src/config/serve.rs`, `src/mcp/serve.rs` |
| `DEFAULT_IDLE_DETACH_SECS` | 900 | `src/config/serve.rs` |
| `DEFAULT_MAX_SESSIONS` | 32 | `src/mcp/serve/http_guards.rs` |
| `DEFAULT_RATE_LIMIT_RPS` | 50 (burst 2×) | `src/mcp/serve/http_guards.rs` |
| `MAX_HTTP_BODY_BYTES` | 4 MiB | `src/mcp/serve/http_guards.rs` |
| `REQUEST_BODY_TIMEOUT` | 30 s | `src/mcp/serve/http_guards.rs` |
| `PINNED_RETRY` | `ELECTION_RETRY` × 5 = 5 s, never under 1 s | `src/mcp/serve/registry.rs` |
| `AT_CAPACITY_RETRY` | 5 s | `src/mcp/serve/registry.rs` |
| `IDLE_SWEEP_MAX` | 30 s | `src/mcp/serve/registry.rs` |
| `ATTACH_FOLLOWS` | 3 | `src/mcp/serve/registry.rs` |
| `MIN_IDLE_DETACH` | 1 s | `src/mcp/serve/registry.rs` |
| `PREVIOUS_HANDLE_WAIT` | 30 s | `src/mcp/serve/registry.rs` |
| `ATTACH_TIMEOUT` | 60 s | `src/mcp/serve/registry/on_demand.rs` |
| `PROBE_TIMEOUT` | 5 s | `src/mcp/serve/registry/on_demand.rs` |
| `ATTACH_WAIT` | 15 s | `src/mcp/serve/registry/on_demand.rs` |
| `ATTACH_BUSY_RETRY` | 5 s | `src/mcp/serve/registry/on_demand.rs` |
| `EVICT_MIN_IDLE` | 2 s | `src/mcp/serve/registry/on_demand.rs` |
| `NEGATIVE_TTL` | 30 s | `src/mcp/serve/registry/on_demand.rs` |
| `NEGATIVE_CACHE_MAX` | 1024 | `src/mcp/serve/registry/on_demand.rs` |
| `UNPLACED_PER_PLACE` | 2 | `src/mcp/serve/registry/on_demand.rs` |
| `ERASE_PERMIT_WAIT` | 10 s | `src/mcp/serve/registry/erase.rs` |
| `ERASE_SHUTDOWN_GRACE` | `CLOSE_FLUSH_GRACE` = 8 s | `src/mcp/serve/registry/erase.rs` |
| `MAX_ADMIN_BODY_BYTES` | 1024 | `src/mcp/serve/admin.rs` |
| `MAX_ADDRESSED_LEN` | 128 | `src/surface/session.rs` |
| `LEASE_TTL` | 45 s | `src/store/lease.rs` |
| `PROBE_RETRY_BACKOFF` | 60 s | `src/writeq/calibration.rs` |
| `HUB_ERASED_CODE` | -32003 | `src/mcp/proxy/disconnect.rs` |

## Rules other work relies on

### The inherited-credential rules (#4 PR 3, #90)

`serve-web`'s `inherit_serve_credentials` imports every `[[serve.credential]]`
as a read grant. It relies on these #32 rules. A change to any of them
changes what a portal token can read:

1. **One grammar.** `[serve]` and `[web]` credentials share
   `config::credential`. That covers reserved names (`default`, `local`), the
   inline-token refusal, the `token_env` rule (an upper-case name of at most
   64 bytes that is not token-shaped, and never `LAMBO_AUTH_TOKEN`), the
   strict charset for scope entries, duplicates, and the startup set check
   (`config::check_credential_set`: a name or token used twice, or a token
   equal to the legacy one).
2. **What `"*"` means on serve.** It means the pinned `[serve] sessions` plus
   every name under any `[[serve.credential]]` prefix
   (`ServeConfig::hosted_sessions()`). An unpinned exact name that some
   other credential lists is not in it. The portal pins an inherited `"*"`
   to that set (`SessionScope::star_within`), never to `[web] sessions`.
   On-demand attach (PR 6) changed *when* serve holds a session, not *which*
   sessions `"*"` may address, and that is why the static expansion is
   correct. If serve's `"*"` ever widens, for example to sessions pinned
   only by `--session`, which the file cannot see, the portal's import must
   follow.
3. **Capabilities do not carry over.** An inherited grant gets
   `SessionCapabilities::default()`: `create`, `erase` and `admin` are
   dropped, and the portal refuses those keys in `[[web.credential]]` by
   presence.
4. **Constant-time scan over every grant.** It is the same
   `surface::bearer::match_any` over one `SessionAuthority`, in a set order:
   `default`, then `[[web.credential]]`, then the inherited entries.
5. **Implicit `local` only when the set is empty.** One inherited credential
   ends the unauthenticated loopback portal and turns its Host check off, as
   on serve.
6. **Secrets are read only when used.** With inheritance off, the portal
   never reads serve's variables. A stdio serve never reads any of them.

### For #33 (the rig's cut-over) and later work

- `/mcp` serves only a pinned default. A client with only a global config
  lands there.
- The memory-protocol hooks key on the server name, not the URL, so moving
  a client to `/mcp/s/<project>` does not affect them.
- There is no operator detach route yet (above).

## The startup notice

`SERVE_UNENFORCED_NOTICE` still reads "parsed but not yet enforced for some
keys in this release". Since PR 6, the only key it can name is
`[[serve.projects]]` on an HTTP serve. That key is never read over HTTP by
design, not "not yet enforced". The wording is a log string that a test
checks, so PR 9 (docs only) left it unchanged. Rewording it ("ignored by an
HTTP serve") is a small follow-up.

## Deviations from the design, in one table

| design | design said | shipped | why | PR |
|---|---|---|---|---|
| §3.1 | `ProcessTasks` in `shutdown.rs` | in `process.rs` | start and stop of the process tasks in one file | 2 |
| decision 14 | probe once per process | once per embedder; a failed or aborted probe is retried after 60 s | one failed remote probe would otherwise disable probe telemetry for the life of the process | 3 |
| §2.5 | every legacy serve is a one-session registry | the one-session path keeps the J2 election and proxy, then wraps the holder in a registry | J2 behavior byte-identical | 4 |
| §3.1 | slots `Attaching`, `Live`, `Detaching`, `Erasing`, `HeldElsewhere`, `Erased` | plus `Failed` (503, no `Retry-After`) | a non-transient attach error must not retry forever | 4 |
| Q2 / §2.1 | `/mcp` is the default: first `--session`, else `default_session`, else the first pinned | the same, and the default must be pinned | `/mcp` must never attach on demand | 4, 6 |
| §3.6 | global rate bucket; process-wide `max_sessions` | a bucket per credential; a per-credential share `max(1, floor(cap / n))` of `max_sessions` | fairness between credentials; the floor keeps every share inside the cap | 5 |
| §3.2 | capacity and eviction before the attach | probe first, then reserve a place and evict | no eviction for a request that will not attach (review H1) | 6 |
| §3.1 | `Erased` slot as an LRU negative cache | `NegativeCache` for `Absent`, `Erased` and `Failed`, 30 s, 1024 entries; `Slot::Erased` for pinned ids only | repeats must not queue on the SQLite permit; the map must stay bounded under a Keel fan-out | 6, 7 |
| §3.2, §3.6 | no timeouts, no eviction floor, no shares of places | `ATTACH_TIMEOUT`, `PROBE_TIMEOUT`, `ATTACH_WAIT`, `EVICT_MIN_IDLE`, the unplaced cap, `place_share` | the Opus review of PR 6 (M1, M2, M4, L-b) | 6 |
| §3.6 | `per_session_rps` defaults to the global rate everywhere | a one-session serve with nothing on demand draws a per-session bucket only when the key is set | a one-session serve answers exactly as before | 6 |
| §6.3 step 2 | end the MCP sessions, erase as the holder, then fence and tear down | fence, close without flush or release, join the tasks, then erase as the holder | nothing of the session runs during the erase; closes the recall-index mirror window | 7 |
| §6.3 | (none) | the erase takes every attach permit within `ERASE_PERMIT_WAIT` (10 s) before it claims the slot | no retry can admit over `Erasing`; a FIFO semaphore must not stall the admin request | 7 |
| §6.2 | erased: `-32003`, or 410 on admin routes | `-32003` to a POST with one JSON-RPC request; 410 to everything else; `Erasing` is 503 | only a request with an id can take a JSON-RPC error | 7 |
| §6.3 | `POST /admin/s/{s}/detach` | not served | a pinned detach is re-elected within 5 s; it needs operator-detach semantics first | 7 |
| §6.3 | `/admin/sessions` reports `last_used` | not reported; sizes and `log_depth` are | not added when PR 6's activity tracking landed | 7 |
| §2.2 (spec) | one process owns a session | one process may own many sessions, each with exactly one writer | see the deviation note | 4 to 7 |

## Not done, and where it goes

- MCP `roots` binding for global-config HTTP clients (Q10): a follow-up
  issue.
- A merged recall across sessions (Q11): its own issue. v1 uses a
  `general` session that clients add as a second server.
- A memory-byte budget (Q12): bytes are reported, not enforced.
- Batched lease heartbeats, and a refusal query across sessions (R1):
  measure first.
- An operator detach route (above).
- Grouping the observability kit by `session`, and splitting the rig's
  session: #33.
- Recall-permit fairness per credential on `serve-web`: an open question in
  the #4 PR 3 note.
- An exact `=3.1.2` rmcp pin (PR 5 note N2). The code rests on rmcp facts
  that a source test pins. The pin is a dependency change, so it needs the
  owner.
- The startup notice's wording (above).
