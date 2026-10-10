# #32 PR 7: the admin surface and the in-serve erase (decisions)

Base: main `5773e4a` (#32 PRs 1 to 5 and 8, #23 erase merged). Branch
`feat/32-pr7-admin-erase`. Design of record: the approved #32 design, §6.3
(erase from a running serve, Q6 and Q7), §6.4 (what stays CLI-only), §4.5
(the #23 tombstone and the registry), §6.1 and §6.2 (the `erase` and
`admin` flags, the uniform 404), and the PR 7 row of §8, the acceptance
list. Decisions and why; the commits carry the mechanics.

#23 shipped erasure as a CLI verb only and deferred any wire surface to
#32's authority design. With PR 5's credentials in place, PR 7 lifts that
for one route on `lambo serve`, as the design says. Everything §6.4 names
stays CLI-only, and the wire never lifts a tombstone.

## Module map

| module | holds |
|---|---|
| `mcp/serve/admin.rs` (new) | the two routes, `POST /admin/s/{session}/erase` and `GET /admin/sessions`, and their status mapping |
| `mcp/serve/registry/erase.rs` (new, child of `registry`) | `SessionRegistry::erase` and `EraseAnswer`, the attached and unattached flows, the startup and retry classification by tombstone |
| `mcp/serve/registry/views.rs` (new, child of `registry`) | `SlotView`, what `/admin/sessions` reads |
| `mcp/serve/registry.rs` | `Slot::Erasing`, `Slot::Erased`, `Lookup::Erased`, the shared-store cell, the retry's two guards (additive) |
| `mcp/serve/transport.rs` | `Lookup::Erased` is `410`; `http_app` merges the admin routes behind the same guard layer |
| `mcp/serve/http_guards.rs` | `/admin/` requests are never MCP-session openers |
| `memory/leases.rs` | `Memory::fence_for_erase`, `Memory::lease_holder` |
| `memory/shutdown.rs` | the fenced close joins what it aborts; names an erase |

The registry's new code sits in child modules so the changes to
`registry.rs` itself stay small and additive (PR 6 changes the same file in
parallel; see "For the merge with PR 6").

## Decisions

**Authorization before anything, through PR 5's authority.** The erase
route runs `SessionAuthority::authorize(grant, raw, SessionNeed::Erase)`:
the raw path segment's shape (never percent-decoded), then the scope, then
the capability, in memory. Each of these refusals is the uniform 404, byte
for byte the unrouted path (the bearer check before them answers 401), and
the store is not called; the body is not even read. Only inside scope do
405, 400, 409, 410 and 503 appear. `/admin/sessions` needs `admin`, then
filters its rows by the same scope test (`SessionNeed::Admin`), with a
loose-named pinned session (one-session serve) visible only to a scope
over every pinned session, as `/mcp` authorizes it. The implicit `local`
and legacy `default` credentials carry neither flag, so wire erase always
needs a configured credential, loopback included (Q5).

**Order of the attached erase: fence and quiesce first, then erase as the
holder.** Design §6.3 step 2 says: end the MCP sessions, erase as the
holder, then fence and tear down. PR 7 fences and tears down first:
slot to `Erasing`, stop the lease watcher (so the fence is not booked as a
lost lease and does not spawn a detach), `Memory::fence_for_erase` (the
lease-lost latch with the tombstone as winner, in-process only), end the
MCP sessions, `close_bounded` (fenced: pipeline quiesced, writers gate
drained, heartbeat and every task aborted and joined, tail discarded, lease
**not** released), abort the event pump, release the endpoint, then
`store.erase_session(id, mem.lease_holder())`, then `Erased`.

Why: both orders keep the design's reason for erasing as the holder (no
lease gap, Q7), because neither releases the lease. Fencing first also
makes "nothing of this session runs during the erase" true by construction
rather than by the store fence alone:

- an in-flight write either finished before the writers gate closed (its
  mutations are in RAM and discarded, or already flushed and erased) or is
  refused with the erased error;
- the write queue (#11) is quiesced and its replay stopped; any intent rows
  (#22 image intents included) that were flushed are erased with the
  session, and no next attach can replay them (the tombstone);
- receipts: the session's routes answer 410, so no receipt can be polled;
- a flush in flight is aborted **and joined**, so it cannot commit and then
  mirror into the #18 recall index after the tiered erase swept it (the one
  hazard the store fence does not cover, since the mirror follows a
  successful primary write);
- the #14 recall cache, the access dirty set (#30) and the graph's vectors
  (#8) are per-`Memory` and go when the handle drops (tested: the `Weak`
  dies).

The lease row stays this process's throughout (the heartbeat stops at the
close, at least 30 s of TTL remain), so the #23 gate admits the eraser's own
live lease. If it lapsed and another writer took the session meanwhile, the
erase answers 409 and the slot returns to `HeldElsewhere`.

The cost: an erase that fails before its commit has already discarded the
RAM tail, which the caller was deleting anyway. The lease is then released
(holder-scoped, bounded) and a pinned slot goes back to `HeldElsewhere`
with the old handle as `previous`, so the retry loop serves it again from
what is durable. If the store reports an error but the lease row reads back
as the tombstone (the durable erase committed, a later step such as the
recall index sweep failed), the slot is `Erased` and the 500 says to repeat.

**The fenced close now joins what it aborts.** `close()`'s lease-lost
branch aborted the canon, daemon and flush tasks and returned. `abort()`
returns before the task has stopped (R3-1); only the join proves it. Fixed
in its own commit for every fenced close, since a lost-lease close has the
same mirror window in principle.

**The erase runs on a task the shutdown waits for.** `SessionRegistry::erase`
spawns its body and records the task with the detaches, so a client that
hangs up cannot cancel it between the fence and the commit, and the
process shutdown's stage 3 waits for it (bounded by `CLOSE_GRACE`). An erase
requested once the shutdown has taken the attached set answers 503.

**Unattached sessions are erased as the CLI erases them**, with the CLI's
eraser identity (`lambo-erase-session@host#pid`), under the attach lock so
the pinned retry cannot interleave, and the prior slot is restored if
nothing was erased. A live holder elsewhere is 409 (the CLI's exit 1).

**Slots.** `Erasing` and `Erased` both make `lookup` answer
`Lookup::Erased`, which the MCP routes render as `410 Gone` (in scope
only). An HTTP status, not the design's MCP error `-32003`, for the same
reason the other non-serving states are 503s: the request may carry no
JSON-RPC id, and no MCP session of the erased session survives. A tool call
already inside the session when it was fenced gets the #23 erased error.
`Erased` is kept only for hosted (pinned) ids; an erased id outside the
hosted set leaves no slot, so the negative cache is bounded by the hosted
set (a Keel fan-out over thousands of users must not grow the map). The
store's tombstone refuses any later attach either way.

**Erased pinned sessions at startup and on a retry.** PR 4 made an erased
pinned session a startup error and a retry that met the tombstone `Failed`.
Now both are `Erased`, decided on the lease row (`is_tombstone`), never on
the error text: the other sessions start, and the erased one answers 410.
The one-session serve keeps refusing to start on an erased session (there
is nothing to serve; it goes through the election, unchanged).

**A one-session serve exits after erasing its session.** Its policy is
`ExitProcess`: the fence arm in `wind_down` fires on the erase's fence,
books the `lease` `lost` line naming `lambo:erased`, now logs "this session
was erased; exiting", and the transport's graceful drain lets the in-flight
erase response go out (the erase task is waited for in stage 3). Tested
across the process boundary, with the restart refusing.

**`/admin/` is not an MCP opener.** The guard buffered and cap-reserved
every sessionless POST whose body did not parse as an `initialize` (it
over-counts on purpose). An erase POST would have been refused at the
session cap. `opens_a_new_session` now excludes the `/admin/` prefix; the
route reads its own body (1 KiB, `REQUEST_BODY_TIMEOUT`) after
authorization.

**`POST /admin/s/{s}/detach` is not served.** Design §6.3 lists it for
#33's cut-over. A pinned session's detach ends in `HeldElsewhere` and the
retry loop takes it back a few seconds later, which makes an operator
detach pointless until PR 6 changes detach semantics (an on-demand detach
removes the slot). Left to PR 6 or 9.

**`/admin/sessions` fields.** `session`, `state` (`live`, `detaching`,
`held_elsewhere`, `failed`, `erasing`, `erased`, `unattached`), `pinned`,
`default`, and for a live session `attached`: node, edge, concept and
embedded-concept counts, `log_depth`, and `estimated_vector_bytes`
(embedded concepts × dim × 4, the f32 payload #8 keeps in RAM; graph
structure is not counted). `last_used` is not reported: activity tracking
arrives with PR 6. The sizes are read after the slots lock is dropped, so
no graph lock is taken inside it.

## Tests

- `memory::tests::leases::fence_for_erase_closes_without_flushing_or_releasing_and_the_holder_erases`.
- `mcp::serve::tests::registry::erase` (in-process, real router and guards,
  recording store with a new flush gate): the attached erase with a flush
  parked inside the store (census, no release, 410, no re-attach, handle
  dropped, other session serving, repeat `already_absent`); the unattached
  erase over a planted session of every table kind (an image intent
  included); 409, 400 (five bad bodies, an unknown field), 405, 503; the
  byte-identical 404 with no store call for missing capability and out of
  scope on three methods and eight ids, and for `/admin/sessions` without
  `admin`; the scoped listing; the tool list identical across credentials
  with no erase tool (the schemas themselves stay pinned by
  `published_tool_schemas_are_pinned_to_the_golden`, unchanged); a held
  session erased meanwhile becomes `Erased`.
- `tests/serve_admin_erase.rs` (spawned, SQLite): a two-session hub erases
  a written and flushed session; the DDL census is the tombstone alone
  before and after SIGTERM; the other session keeps its rows. A one-session
  hub answers, exits, and a restart refuses.
- Mutations: an unfenced close before the erase fails the attached test
  (the lease is released first, `leases: 0`); authorizing with
  `SessionNeed::Use` fails the 404 test. Skipping the fence and close
  entirely is now caught too (Opus review M2, below): the recording store
  snapshots its parked-flush gauge when `erase_session` is entered, and the
  test asserts it is zero.

## Merged with PR 6 (main 8e47697d)

PR 6 landed first. How each conflict was resolved:

- `registry.rs`: PR 6's attach permits, `Slot::Attaching { placed, .. }`,
  `answer_for`, `owners`, `negative`, `track` and `on_demand.rs` taken as
  they are; PR 7's `Erasing`/`Erased`, store field and retry guard kept on
  top (the guard runs after the permit). `answer_for` gained the arms.
- `transport.rs`: one `Lookup::Erased` arm (PR 6's 410 body), now
  `erased_answer` (below).
- `views.rs`: an `Attaching` slot lists as `attaching`.
- `tests/serve_admin_erase.rs`: the one-session test scopes both
  credentials to its session. With PR 6, a credential reaching past the
  pinned set makes the serve attach on demand (`DetachSession`), and the
  erase then no longer ends the process.
- CHANGELOG, `cli.mdx` (and the site mirror), `serve.rs`, the registry
  test modules: both sides, main's first.

## Opus review remediation (2026-10-10)

PR 6 reconciliation, as the review listed it:

1. The erase uses PR 6's permits: it takes **every** attach permit
   (`acquire_many(permit_count)`, raced against the shutdown) before it
   claims the slot, and drops them right after the claim. Holding them
   through the store erase was not kept: once claimed, `Erasing` keeps
   every attach of the id away (the retry's guard, `answer_for`, the
   conditional admissions), and holding them would stall every other
   attach and the shutdown's `close_set` behind the store call.
2. `Attaching` (and `Detaching`, `Erasing`) answer the erase 503.
3. `answer_for`: `Erasing` is `Unavailable { 1 s }` (L3), `Erased` is
   `Lookup::Erased`.
4. `takes_a_place` counts `Erasing`: the handle is closed only after the
   claim, so it holds memory. An `Erasing` slot of an unattached id is
   over-counted for a few store round trips; that only refuses an attach.
5. Eviction (`choose_victim`) and the idle sweep take `Live` only; a test
   pins that `Erasing` is neither.
6. One erased answer (below) for the probe's `AttachOutcome::Erased`, the
   negative cache and `Slot::Erased`. An erased id that is not pinned keeps
   no slot and no owner: `mark_erased` puts `Negative::Erased` in PR 6's
   cache (no separate erased cache), and the probe answers from the
   tombstone once it expires. Pinned is `is_pinned`.
7. The erase keeps its own `erase_attached` (it fences before the drain
   and erases as the session's holder, which `run_detach` does not), with
   PR 6's conditional style: every final slot write happens only while the
   slot is still `Erasing`. A failed on-demand erase leaves the slot and
   owner removed and the old handle in `previous`, as a detach does.
8. The erase task is recorded with `track_unless_closing`.
9. The retry guard survived the merge, and the retry's post-acquire writes
   are conditional (H1).

Findings:

- **H1.** Permits before the claim (above). Every background admission is
  conditional: `admit_if`/`insert_live_if` check the slot under the lock
  (the pinned retry: still `HeldElsewhere`; the on-demand attach: its own
  `Attaching`), and a refused admission closes what it built, releasing
  the lease. The retry's `Erased`/`Failed`/`HeldElsewhere` writes go
  through `replace_held`. Tests: the erase racing a retry parked in its
  load waits for it and erases what it attached; a retry whose slot was
  changed releases its lease.
- **M1.** `close_bounded`'s `Config` error is the abandon (the fenced
  branch returns the erased `Store` error); on it the erase calls
  `Memory::stop_tasks_for_erase` (heartbeat, replay, canon, daemon, flush:
  aborted and joined, no writers gate, no close lock). The fenced branch
  shares the abort-and-join helper. Test: the writers gate held and two
  signals recorded, then no flush parked at the erase and none completing
  after it.
- **M2.** The recording store's flush gauge, filtered to one session's
  batches (`park_flushes_of`), snapshotted at `erase_session` entry, and a
  `flushed` note after each flush that passed the store. Mutation-checked.
- **L1.** `track_unless_closing` (check, spawn, record under the list's
  lock). Once the shutdown has taken the set, the erase gets
  `ERASE_SHUTDOWN_GRACE` (= `CLOSE_FLUSH_GRACE`, asserted under
  `CLOSE_GRACE`) and is then cut short with an unknown outcome. The erase's
  fence latches quietly (`LeaseLostSignal::record`) and is announced on
  every way out of `erase_attached` (`AnnounceOnDrop`), so a one-session
  serve's wind-down, and its transport drain, start only after the erase
  has its answer.
- **L2.** `store::erase::{read_tombstone, Tombstone}`; `refused_as_erased`
  and the registry use it (the duplicate check is gone). `Unknown` answers
  500 "the outcome is unknown; repeat the request".
- **L3.** `Erasing` answers 503 `Retry-After: 1`; 410 is for `Erased`.
- **L4.** `SessionRegistry::new` takes the store (the template's, or the
  one-session serve's session's); the `OnceLock` is gone. Not reachable
  before either: a no-attacher registry admits its session before routes.
- **L5.** The listing runs on the blocking pool. No test pins it: holding
  a graph lock to show the runtime stays free also blocks the session's
  own flush and daemon tasks on the runtime's threads.
- **Nits.** 408 in the route table and the docs; "refused after 1 KiB";
  the duplicated tombstone check removed; the supervised one-session crash
  loop and the prefix credential's tombstones documented. The one-session
  serve's ledger still books `lease` `lost` with the tombstone holder as
  winner, the line #23's heartbeat-detected erase books: a distinct
  `lease:erased` event would be a new ledger vocabulary entry, and the
  winner already says it was an erase.
- **MCP error -32003** (design §6.2). It fits: the erased answer never
  reaches rmcp, so `transport::erased_answer` reads the body (the guard's
  ceiling and timeout) and answers a POST whose body is one JSON-RPC
  request with a `method` and a non-null `id` (`initialize` included) with
  the proxy's own erased frame (`proxy::erased_reply`, 200, JSON);
  everything else with no id to answer (GET, DELETE, a notification, a
  response, a batch, an unreadable body) keeps 410.

## Not run here

The Postgres and Cockroach live legs; nothing in PR 7 changes a store.
