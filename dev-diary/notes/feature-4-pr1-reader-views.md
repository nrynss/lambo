# #4 PR 1: per-session reader views for `serve-web` (decisions)

Base: main `2221fe4`. Design of record: the approved #4 design
(`4-DESIGN.md`, owner-approved 2026-10-09), sections 2.3, 2.4, 5, 7, 8, the
PR 1 row of section 9, section 10, and Q3, Q13, Q15 and Q17. Decisions and
why; the commits carry the mechanics.

PR 1 is still **single-session**. It changes how the portal reads, not what it
serves: no routing, allowlist, credential or Host-check change (PR 2 and 3).
The view cache is keyed by `SessionId` with one slot per served session, so
PR 2 serves an allowlist through it without rework.

## What changed

| piece | where |
|---|---|
| `ViewCache`, `SessionView`, `ViewBounds`: TTL, single-flight, LRU, load and recall semaphores, per-session freshness and query cache | `src/cli/serve_web/views.rs` |
| `AppState` holds the cache instead of a freshness tracker; `AppState::new` | `src/cli/serve_web/state.rs` |
| feed split into `ordered_events` + `slice_events`; stats from the view's counts | `src/cli/serve_web/projections.rs` |
| every data route reads `state.view()`; recall permit and 503 | `src/cli/serve_web/routes.rs` |
| startup: schema preflight, lazy sessions, bounds line | `src/cli/serve_web.rs` |
| `RecallRequest::validate`, `run_detailed_on` (optional query cache) | `src/cli/recall.rs` |
| `[web]` table | `src/config/web.rs`, `src/config.rs` |
| `[web]` read from the single `lambo.toml` read | `src/main.rs` |
| store-call accounting, TTL, single-flight, failure, erase, LRU, recall-parity, 503, query-cache tests | `src/cli/serve_web/tests/views.rs` |
| docs | `docs/reference/{cli,config}.mdx` and site mirrors, `lambo.example.toml`, `CHANGELOG.md` |

## Measured

Store loads per request, from the counting store (`tests/views.rs`). The
per-route rows are measured on both sides; the two aggregate rows are measured
with views and computed from the per-route costs before:

| | before (main `2221fe4`) | after the fix commit | with views |
|---|---|---|---|
| `/api/pulse` | 2 | 1 | 1 per session per TTL, any number of tabs |
| `/api/stats` | 2 | 1 | shared with the pulse |
| `/api/events`, `/api/session`, `/api/graph`, `/api/inspect`, `/api/recall` | 1 each | 1 each | shared with the pulse |
| 8 concurrent pulses on a cold view | 16 (computed) | 8 (computed) | 1 (measured) |
| a page open (session, pulse, graph, events, stats, inspect, recall, pulse) | 11 (computed) | 8 (computed) | 1 (measured) |

Wall time, release build, in-RAM store, a 3,601-concept fixture session
(`seed_chain_around(.., 3600)`), mean of 20 (a scratch test, not committed):
the old pulse's two loads 11.4 ms; one reader load 11.0 ms; a pulse at
`view_ttl_ms = 0` (one load per request) 11.6 ms; a pulse served from a live
view 0.17 ms. The in-RAM store's raw snapshot is nearly free (a clone), so
there the second load cost only 0.4 ms. On SQLite the raw load is the expensive
part (#8 measured 282 ms for a 3,600-concept load), so there the old pulse paid
it twice. SQLite and Postgres wall times were not measured here.

## Decisions

**The feed comes from the loaded graph, not from a second snapshot.** The
design (5.1) asks for a `load_reader_graph` variant that returns the snapshot's
events. Not needed: `Graph::from_snapshot` keeps every concept and every
canonization event, and `Graph::canonization_events` exposes them, so
`ordered_events(g.concepts(), g.canonization_events())` builds the same feed
from the one load. A test pins the view's feed equal to `events_from` over the
store's own snapshot. Side effect: a snapshot whose events name another session
now fails `/api/events` too (the graph refuses it), where before only the
pulse failed. That is a corrupted store either way.

**Single-flight without `futures::Shared`.** No new crate (Cargo.toml is
frozen). Each slot has a tokio `Mutex<()>` gate and a `completed` counter. A
request reads the counter on arrival, queues on the gate, and if a load finished
while it queued it takes that load's outcome: the view, or the error. So a burst
is one load even at TTL 0, and a dead store is hit once per burst, not once per
queued request. Failure is not cached: the next request after the burst loads
again, behind the load semaphore.

**Eviction drops the view only.** Slots are never removed (one per served
session, a fixed set). The freshness tracker (`durable_change_age_ms`) and the
query-embedding cache live on the slot, so a reloaded session does not report
"just changed" and does not re-embed. A request holding an evicted view keeps
its `Arc`, so the LRU count is not the whole bound: views held by requests in
flight are outside it. A recall holds its view across the embed (possibly
remote) and the pipeline, so roughly `max_loaded_sessions + load_concurrency +
recall_concurrency` views can be alive at once, plus any a structural route is
briefly serializing. Still count-bounded, not byte-bounded.

**Counts are taken once per view.** `/api/stats` and the pulse used to walk
every concept on every poll; the view stores `nodes/edges/concepts/canonical`.
The fingerprint and the freshness age are still computed per request.

**Startup: schema preflight, then lazy (Q15).** `run` no longer loads the
session. It calls `preflight_schema()` so an unprovisioned store still fails
startup (a test pins it, with zero loads). Every view load preflights again,
as `load_reader_graph` always has. The H1 mismatch warning moves to the first
load that sees a mismatch, once per session, naming the session.

**Recall order: validate, view, permit, run.** `RecallRequest::validate` holds
every usage refusal, so a bad query is a 400 with no permit wait and no store
call (the existing `recall_endpoint_rejects_a_missing_query_without_touching_the_store`
still passes). The permit (2 s wait, then 503 + `Retry-After: 1` + `no-store`)
is taken after the view. The first draft took it before, "so the bound covers
the load a recall may trigger"; review L5 showed that a slow load then holds a
recall permit for its whole wait (up to 30 s on Postgres), so a few recalls on
one slow session answer every other session's recall 503. Loads are already
bounded by the load semaphore, and design 5.3 bounds recalls for embed and
pipeline work, so the permit now covers only that.
`run_detailed_on` asserts the embedding contract on the view it is given, the
same assert and message as `load_reader_graph_with_contract`; `lambo recall`
now loads without a contract and asserts in `run_detailed_on`, same order and
text. Recall parity: `context`, `response_annotations` and `hits` are
identical to `run_detailed`, scores included, with no tolerance. Nothing on the
recall path reads the clock (a reader daemon's score is 0, BM25 sums in sorted
term order, session recency is anchored to graph timestamps). An earlier draft
of the test allowed 1e-6 on `score` and blamed a wall-clock recency term; that
was wrong. The one-ULP difference it saw was serde_json's float parse (the
crate does not enable `float_roundtrip`) on the HTTP side only, so the test now
round-trips the CLI's hits through a JSON string and compares exactly.

**#14 per session.** The portal embeds through `embed_query_cached` over the
slot's `QueryEmbeddingCache`. Never process-wide (#32 decision 13). The
pipeline `RecallCache` stays per recall: a reader graph is always epoch 0.

**`view_ttl_ms` is capped at 60 s.** Not in the design. A view older than a
minute is a frozen page, not a cache; 0 to 60000 covers every sensible
setting. Refusals name the key and the bound, never the value (#32 PR 1 rule).

**`load_concurrency` is forced to 1 on SQLite** in `ViewBounds::resolve`,
silently (the startup line prints the effective value).

**No background task, no `.spawn()`.** The reader source scan bans it; a
refresh runs on the requesting task.

## Tests changed, and why

- `feeds::events_endpoint_tails_the_canonization_feed` and
  `session::h1_live_contract_changes_update_session_pulse_and_keep_recall_fail_closed`
  write to the store between two requests and assert the second sees the
  write. That is a property of the store read, so they now run at
  `view_ttl_ms = 0` (`tests::web_ttl_zero`). The TTL itself is pinned by
  `views::a_write_is_served_after_the_ttl_and_not_before`, on `ViewCache`
  with tokio's paused clock (design section 10): reused at `ttl - 1 ms`,
  reloaded at `ttl`. `views::the_pulse_serves_a_write_once_the_ttl_has_passed`
  keeps a real-clock route check that asserts only "served after the TTL".
- The burst tests (concurrent pulses, TTL 0, failed load) no longer rely on a
  50 ms store delay. The test store parks the session's loads, the test waits
  until every request has queued for the view (a `cfg(test)` counter on the
  slot), then releases one load. A request that would start a second load
  fails on a 30 s guard instead of hanging.
- The two `AppState { .. }` literals in `session.rs` and the one in
  `tests/mod.rs` became `state_from_backends` / `state_with_web` calls:
  `AppState` lost its `freshness` field and gained the cache.
- `PRODUCTION_SOURCES` lists `serve_web/views.rs` (graph constraint
  `0f076ab8`).

Mutation checks run: removing the gate and the joiner rule fails the
concurrent-pulse, TTL-0 and failed-load tests; making every view stale fails
the one-load-per-TTL, TTL, LRU and concurrent tests.

## Not done here

PR 2: allowlist, `/s/{session}` routing, `SessionAuthority`, the Host check.
PR 3: per-credential scope and listing. The byte estimate per view is logged at
`debug` on each load, not enforced (Q17). A request-rate limiter stays out (Q13).
