# Adversarial Review: T8.6 — Single-writer lease (store-enforced §2.2)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: R2 REMEDIATED — all 6 findings fixed/documented          ║
║  Verdict: central safety property HOLDS; 3 P2 + 3 P3 findings    ║
║  Central claim tested: "can two writers ever hold the lease on   ║
║    one session at the same time?"  → NO (80/80 cross-process     ║
║    trials clean; all 3 safety mutations caught by tests)         ║
║  P1: none                                                        ║
║  P2: 3 → all FIXED (T86-2 fence lost-lease writer; T86-1 acquire ║
║        before load; T86-3 cockroach acquire in tx_retry)         ║
║  P3: 3 → T86-6 FIXED (winner-liveness pinned); T86-4/T86-5       ║
║        documented (accepted residuals w/ operator override)      ║
║  Remediation: task/t8.6-lease, 5 fixes @ b4f1a6b + this doc      ║
║  Opened: 2026-08-14 · Remediated: 2026-08-14                    ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T8.6 — Single-writer lease, store-enforced (spec §2.2; PHASE-8-surface.md)
**Branch reviewed:** `task/t8.6-lease` @ `61734df` (6 commits over `9d314a2`)
**Scope:** `src/store/lease.rs`, lease methods in `src/store/{mod,memory,sqlite,cockroach}.rs`,
`build()`/`close()`/heartbeat/`Drop` wiring in `src/memory.rs`, serve-path release in
`src/mcp/serve.rs`, `migrations/{sqlite,cockroach}/001_init.sql`, `scripts/provision.sh`,
`tests/serve_single_writer_lease.rs`.
**Method:** clause-by-clause read against the T8.6 task block and the six Handoff-Log probes;
a **true cross-process concurrent-acquire hammer** (80 + 12 paired `lambo serve` launches on one
SQLite file/session); **raw-SQL verification** of the `ON CONFLICT … WHERE … RETURNING`
empty-row semantics on the bundled SQLite behaviour; and **3 source mutations** to measure what
the suite pins. All mutations reverted; `git status` clean and HEAD `61734df` restored before
writing.

**Gates verified independently on the clean tree:**
`cargo fmt --all -- --check` clean ·
`cargo clippy --all-targets -- -D warnings` clean ·
`cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings` clean ·
`cargo clippy --all-targets --features store-sqlite -- -D warnings` clean ·
`cargo test --lib` **598 passed, 1 ignored** ·
`cargo test --features store-sqlite` **641 lib + 2 bin + all integration (incl.
`serve_single_writer_lease`), 0 failed, 1 ignored**. Matches the Handoff-Log baseline exactly.
The one ignored test is the live-cockroach lease test (no `LAMBO_COCKROACH_DSN`).

---

## The central property — DID NOT BREAK

**Can two writers ever hold the lease on one session simultaneously? No, in every test I could
construct.**

- **True cross-process hammer (CONFIRMED).** A bash harness (`scratchpad/t86-review/race.sh`)
  launches two `lambo serve` processes against ONE SQLite file + ONE session as simultaneously
  as the shell allows, with the winner held alive ~1.5 s (stdin open) so the loser's acquire is
  *guaranteed* to overlap the winner's live lease. **80/80 trials: exactly one process exits 0
  (served + released cleanly) and one exits non-zero. Never both, never neither.** Schema was
  pre-provisioned per trial (`lambo provision` is a stub — see note below) so neither process
  raced on `CREATE TABLE`.
- **Empty-`RETURNING` interpretation is correct.** The task flagged "what does the code do when
  `RETURNING` yields no row?" I verified the raw statement on SQLite directly: a fresh/expired/own
  upsert returns the row (→ `Acquired`); a contended live-foreign lease returns **zero rows**, and
  the stored holder is unchanged (→ read-back → `Held`). `acquire_or_refresh` reads `Some(row) ⇒
  Acquired`, `None ⇒ read back ⇒ Held` (`sqlite.rs:984-1005`, `cockroach.rs:1278-1307`). No path
  misreads "no row" as `Acquired`.
- **All three safety mutations are caught by tests** (scorecard below).

The remaining findings are about the *quality of the refusal*, a *store-outage* split-brain
window, and *non-serve* callers — not about the acquire race itself.

---

## Findings

### T86-1 (P2) — Under simultaneous startup the loser's refusal degrades to an opaque `database is locked`, not the designed single-writer message — CONFIRMED (reproduced ~8%)

> **R2 — FIXED** (`f40f415`). Reordered `build()` to acquire the lease **before**
> `load_session` (the reorder option, chosen over error-translation because it is
> clean — nothing in the load depends on the lease and vice-versa). A losing racer
> is now refused at the atomic acquire, before it ever opens the load's
> `BEGIN IMMEDIATE`. Repro re-run (20 paired simultaneous `serve` on one SQLite
> file): **0 `database is locked`**; every genuinely-overlapping loser got the
> honest named refusal; never two holders. A build step that fails after the
> acquire leaves the lease held with no heartbeat → lapses at TTL (documented).

`src/memory.rs:508` (`load_session_async`) runs **before** `src/memory.rs:554` (`acquire_lease`);
the load opens a transaction at `src/store/sqlite.rs:618-626` ("begin load transaction", a
sqlx-sqlite `BEGIN IMMEDIATE` that takes a write/RESERVED lock immediately).

When two `serve` processes attach at the same instant, both enter `load_session_async` before
either reaches the lease. The two `BEGIN IMMEDIATE`s contend on the SQLite write lock, and in a
minority of races the loser's load returns SQLITE_BUSY (`code: 5, database is locked`) rather
than waiting out `busy_timeout`. The process then dies on the *load* path with:

```
lambo serve: backend: begin load transaction: error returned from database: (code: 5) database is locked
```

instead of the designed, actionable refusal that the whole feature is sold on:

```
session … is already held by another writer (agent-a@host#pid) — it acquired the single-writer
lease 0s ago … If that holder is wedged, an operator can force a takeover: DELETE FROM session_leases …
```

**Reproduction:** `scratchpad/t86-review/race_verify.sh`, 12 paired simultaneous launches:
**11/12 losers got the honest lease refusal (named holder + operator override); 1/12 died with
`database is locked` on the pre-lease load.** The 80-trial run counts this as a "clean" pass
because it only checks exit codes — the safety property is genuinely intact (the loser still
fails closed, never acquires) — but the **"one *honest* refusal" half of the "Done when" is not
always met.** An operator hitting the BUSY variant sees no mention of the lease, no holder
identity, and no override hint; it reads like store corruption, not "another writer has this
session."

**Why P2, not P3:** it directly undercuts a stated acceptance criterion ("one honest refusal")
and the feature's entire value proposition (a *clean, named* handoff). A reviewer could
reasonably down-rank to P3 since safety is untouched and the cause is pre-existing load-path
concurrency, not lease code. Remediation options: acquire the lease *before* the startup load
(so the refusal is always the lease's), or catch a BUSY/locked error during load-at-attach and
re-phrase it through the same "another writer may hold this session" lens.

---

### T86-2 (P2) — A holder that LOSES its lease keeps writing; the heartbeat only logs — CONFIRMED (read) / documented-accepted

> **R2 — FIXED** (`d162814`). Took the reviewer's suggested stronger response:
> **latch the session closed on a lost lease**. The heartbeat now sets a shared
> `Arc<AtomicBool>` fence on `LeaseOutcome::Held`; from that instant (1) the write
> gate (`begin_write`/`begin_write_sync`) refuses every `derive`/`record_action`/
> `reserve`/`retract` with an honest "no longer the writer" `Conflict` naming the
> operator override, (2) the write-behind flush loop stops and **drops** its
> not-yet-durable pending rather than overwrite the new holder's rows
> (`FlushTask::with_fence`), and (3) `close()` refuses to flush the tail and does
> **not** release the lease. Silent split-brain → loud, safe stop that drops
> nothing a crash would not have. Two new tests close the scorecard gap: the flush
> loop stops-and-drops on fence (`a_fenced_loop_stops_flushing_and_drops_pending`),
> and a `Memory` that loses its lease refuses all writes + close and never
> persists its tail while the takeover holder's lease stays intact
> (`a_lost_lease_fences_the_writer_and_stops_the_flush`).

`src/memory.rs:320-343` (`spawn_lease_heartbeat`). When a refresh returns `LeaseOutcome::Held`
— i.e. this handle's lease expired (a store outage starved the beat past the 45 s TTL) and
another writer took the session — the code emits `tracing::error!("single-writer lease LOST … the
two writers are now diverging")` and **keeps looping**. Nothing stops the writer: the `Memory`
handle continues to accept writes and `flush()` them into the same rows as the new holder.
`flush()` never consults the lease.

This is *exactly* the corruption the lease exists to prevent — two writers' divergent in-RAM
graphs flushed into one session, later flush wins, one side's GC deletes nodes the other holds —
reached not by a naive double-open (which is refused) but by a store blip longer than the TTL
followed by a legitimate takeover. The task agent flagged this itself (Handoff probe #4) and
documented it as accepted ("recovery is an operator's, not a silent self-destruct that could drop
this handle's tail").

**Why P2:** the module's headline is "the safety mechanism that prevents two processes from
corrupting one session," and this is a path where two processes *do* corrupt one session. The
acceptance is defensible (a self-destruct could itself drop an un-flushed tail), but a
false sense of protection here is the specific hazard the brief calls out. The task agent's own
suggested stronger response — **latch the session closed to writers on a lost lease** (refuse
further writes, force the operator to reconcile) — is the right remediation to weigh: it converts
silent divergence into a loud, safe stop without dropping anything that a crash wouldn't have
dropped anyway. No test exercises the lost-lease path (see scorecard gap).

---

### T86-3 (P2) — CockroachDB lease acquire is not wrapped in the project's `tx_retry`; a 40001 serialization conflict surfaces as an opaque `Backend` error, diverging from SQLite — PLAUSIBLE (code-read; no cluster)

> **R2 — FIXED** (`04bc8d0`). Wrapped `acquire_or_refresh_lease`'s body in
> `tx_retry`, matching every sibling contended write in `cockroach.rs`. A 40001
> (mapped to a retryable `StoreError::Backend`) now replays transparently instead
> of surfacing as an opaque `Backend`/`LamboError::Store`. The inner `for 0..3`
> vanished-row loop is unchanged (orthogonal). No live cluster here — matched the
> existing pattern; compiles + clippy-clean under `--features store-cockroach`.

`src/store/cockroach.rs:1270-1277`. Every other contended write in `cockroach.rs` runs inside
`tx_retry` (5 attempts, backoff, `src/store/cockroach.rs:677-697`) precisely because "sqlx does
not auto-retry" a SQLSTATE 40001 `RETRY_SERIALIZABLE` abort (`cockroach.rs:550-555`). The lease
acquire does **not**: it calls `sqlx::query_as(ACQUIRE_SQL).fetch_optional(pool)` directly, with
only the internal `for _ in 0..3` loop — and that loop retries the **vanished-row** case
(`None` read-back), *not* a 40001. A genuine cross-node acquire conflict therefore maps through
`map_write_err` to a `StoreError::Backend`, which `build()` turns into `LamboError::Store` — an
opaque failure, not the clean `Held` refusal and not a transparent retry.

This is the same *shape* as T86-1 (loser gets an opaque error instead of the designed refusal),
but on the acquire path and on the other backend, so **sqlite and cockroach are not identical
under contention**: SQLite absorbs contention with `busy_timeout(8s)` + WAL and (mostly) yields a
clean `Held`; Cockroach has no equivalent wrapper on this one statement.

**Mitigating (why PLAUSIBLE, not CONFIRMED):** the acquire is a *single* `INSERT … ON CONFLICT …
RETURNING` executed as an implicit transaction, and CockroachDB auto-retries single-statement
implicit txns server-side when it has not yet streamed rows — so 40001 may rarely reach the
client. But this is an unverified assumption (no cluster available), and it is an inconsistency
with both the sibling SQL backend and the rest of `cockroach.rs`. Remediation: wrap
`acquire_or_refresh_lease`'s statement in `tx_retry`, matching every other cockroach write.

---

### T86-4 (P3) — A non-`serve` (library) caller's unbounded `close()` aborts the heartbeat first, so a long final flush can outlive the TTL — PLAUSIBLE (read)

> **R2 — DOCUMENTED** (`b4f1a6b`), the smaller-risk of the two offered options.
> Keeping the heartbeat alive across the flush was rejected: it entangles the new
> T86-2 lost-lease fence with the mid-close release/flush ordering, and this close
> body's ordering is load-bearing for the R2-1/R3-1 cancellation + custody
> invariants — reordering it is exactly the class of change that reopened P2s
> across the T8.1 rounds. Instead added a "Bounding this against the lease" section
> to `Memory::close`'s rustdoc: the method is unbounded and a direct library caller
> MUST cap it below the lease's remaining validity, as `serve` does (`CLOSE_GRACE`
> within `SHUTDOWN_BUDGET`, pinned `< LEASE_TTL` by a `const _` assertion). `serve`
> is the only in-repo production caller; the window is proven closed there.

`src/memory.rs:1444` aborts the heartbeat at the *start* of `close()`, before the flush-task join
(`:1481-1487`, "the whole of close's worst case ≈ 2 minutes") and the final flush (`:1540`). On
the **serve path this is safe**: `close_bounded` caps `close()` at `CLOSE_GRACE`=10 s inside a
`SHUTDOWN_BUDGET`=15 s total, and the build-time assertion `LEASE_TTL(45s) > SHUTDOWN_BUDGET(15s)`
(`serve.rs:93-95`) guarantees the lease is still valid when `release` lands — I walked the timing
and it holds (last beat ≤15 s before signal, +5 s transport +10 s close = ≤30 s < 45 s).

But `serve` is the **only** production `close()` caller. `Memory` is `pub` (a library surface,
`src/lib.rs:36`), and a library embedder that calls `close()` directly gets **no** `CLOSE_GRACE`
bound. With the heartbeat already aborted, a final flush that runs longer than the lease's
remaining validity (≥30 s) lets the lease **expire while the handle is still running its close**,
opening the very "second writer admitted mid-flush" window the serve assertion exists to prevent —
now unguarded. In-repo there is no such caller, so this is latent, but the invariant is pinned by a
`const _` that only covers the serve budget, not the internal ~2-minute worst case. Worth either a
doc-contract on `Memory::close` ("callers must bound this below `LEASE_TTL`") or keeping the
heartbeat alive until *after* the final flush on the non-serve path.

---

### T86-5 (P3) — `Drop` aborts the heartbeat, but a truly leaked handle (`mem::forget` / `Arc` cycle) heartbeats forever and wedges the session — PLAUSIBLE (read)

> **R2 — DOCUMENTED as accepted residual** (`b4f1a6b`). No cheap store-side guard
> exists — a live refresh from a leaked handle is indistinguishable from a healthy
> one. Added a paragraph to the lease module's "Not preemption" note: the
> "leaked/crashed handle lapses at TTL" guarantee depends on `Drop` running, an
> `Arc`-cycle/`mem::forget` leak wedges the session for the process lifetime, and
> the escape is the same `OPERATOR_OVERRIDE` DELETE as any wedged-but-heartbeating
> holder.

`src/memory.rs:1816` aborts the heartbeat in `Drop`, which is what makes a dropped-without-close
handle's lease *lapse* at the TTL (the intended crash-shaped behaviour, Handoff probe #5). That
abort is reliable for an ordinary drop. But if the `Memory` (or its heartbeat's `Arc<dyn
GraphStore>` + session) is retained by an `Arc` cycle or `std::mem::forget`, `Drop` never runs,
the heartbeat task keeps refreshing every 15 s, and the session is **wedged for the process
lifetime**, not just one TTL — with no diagnostic beyond the periodic refresh. This is an
abnormal-leak edge, not a normal path, hence P3; noted so the "leaked handle lapses at TTL" claim
in the module docs is understood to depend on `Drop` actually running.

---

### T86-6 (P3) — The cross-process test only pins the *loser*; the winner's continued clean service through contention is not asserted — CONFIRMED (Handoff probe #6, still open)

> **R2 — FIXED** (`29b2b12`). Extended `tests/serve_single_writer_lease.rs`: after
> B is refused, it drives a `tools/list` over A's transport and requires a result
> (A serving *through* the contention), asserts A exits 0 on a graceful SIGTERM
> close, and confirms a fresh writer can reacquire the lease afterward (A really
> released, not just exited). A mutation making *both* processes fail is now
> caught by the winner-liveness probe, not only the loser-refusal check.

`tests/serve_single_writer_lease.rs` asserts process B fails closed naming the holder, but never
asserts process A keeps serving cleanly and releases on its own exit. My 80-trial hammer covers
the winner-liveness angle empirically (every winner exited 0 = served + released), but there is no
committed regression test for it. Low severity; the empirical coverage exists, just not in CI.

---

## Verified-OK (probed, NOT defects)

- **Sub-second TTL rounding (Handoff probe #2).** `ttl.as_secs_f64()` formats 45 s→`"45"`,
  1 s→`"1"`, 0.08 s→`"0.08"`; SQLite's `strftime('…','now','+0.08 seconds')` yields a *future*
  timestamp (verified on the bundled-equivalent SQLite 3.51 CLI), not one that rounds to 0 / an
  already-expired lease. Production only ever passes 45 s. No defect.
- **Bounded `Held`-retry loop (Handoff probe #3).** `for _ in 0..3` in both SQL backends retries
  only the vanished-row (`None` read-back) case, does real queries each iteration, and returns a
  `Backend` error after exhaustion (`sqlite.rs:1007`, `cockroach.rs:1309`). Cannot busy-spin or
  misreport. No defect.
- **Release-before-durable on a *failed* close (Handoff probe #1 / brief item 3).** On a failed
  final flush, `close()` does NOT release (`sqlite`/memory `release_lease` only runs in the success
  arms, `memory.rs:1531`/`1556`) and the tail returns to the front of the log; the heartbeat was
  aborted, so the lease lapses at TTL. Ordering {stop writers → abort heartbeat → join → drain →
  flush → (success) release} is consistent with "crash-expiry ≠ tail drained; new holder replays
  via startup load." Documented and correct. No defect.
- **Clock discipline / F18 (brief item 6).** No lease timestamp comes from a client argument —
  `acquire`/`refresh` take a `Duration`; each backend stamps from its own clock (Cockroach
  `now()`, SQLite `strftime('now')`, Memory `Utc::now()`). No wire-visible lease field:
  `f18_tool_schemas_match_the_golden_property_set` (`server.rs:1569`) enumerates the exact golden
  property paths for all 7 tools and none is lease-related; the test passes on the clean tree.
- **Atomicity / SQL equivalence (brief item 5).** Both backends: single `INSERT … ON CONFLICT
  (session_id) DO UPDATE … WHERE (expired OR mine) RETURNING`; `session_id` is `PRIMARY KEY` in
  both migrations; expiry compares the **store** clock (`strftime('now')` / `now()`), never a
  client value; release is a holder-scoped `DELETE … WHERE session_id=? AND holder=?`. Semantics
  match except the contention-retry divergence in T86-3.
- **Advisory `ACTIVE_SESSIONS` log retained (brief item 8).** Still fires for the same-process
  *same-agent* double-open (identical token → looks like a refresh, so the lease does not catch it,
  by design — `lease.rs:89-103`). Note: the same-process *different-agent* case is now **refused by
  the lease** (distinct token → `Held`), so the advisory log's live domain narrowed to
  same-agent / cross-store-handle; `a_second_handle_on_one_session_is_reported_loudly` was honestly
  retargeted to two separate stores to keep exercising it. Assertions preserved.

---

## Test-pinning scorecard (3 mutations, all reverted)

| # | Mutation | Property broken | Result |
|---|---|---|---|
| A | `MemoryStore::acquire_lease` guard `… && false` — never refuse a live foreign lease (`store/memory.rs:318`) | live lease can be stolen | **CAUGHT** — 4 tests FAIL (`lease_grants_one_holder_and_refuses_another`, `a_stale_release_does_not_evict_the_new_holder`, `an_unreleased_lease_expires_and_is_reacquirable`, `memory::…a_second_writer_sharing_a_store_is_refused_by_the_lease`) |
| B | `Memory::release_lease_once` → early `return` (no-op on close) (`memory.rs:1660`) | graceful close does not hand off | **CAUGHT** — `mcp::serve::…a_clean_close_releases_the_lease` FAILS (new writer gets Conflict) |
| C | SQLite `ACQUIRE_SQL` guard `WHERE 1=1 OR …` — update always fires (`store/sqlite.rs:972`) | `acquire` always returns `Acquired` | **CAUGHT** — in-process `two_connections_on_one_file_serialize_on_the_lease` FAILS *and* cross-process `tests/serve_single_writer_lease.rs` FAILS (B opens a second writer) |

Every safety property the brief named has a failing test. **No P-level pinning gap** — with one
soft exception: the **lost-lease-keeps-writing** path (T86-2) has *no* test at all, because there
is no behaviour to assert (the writer deliberately does not stop). If T86-2's remediation adds a
"latch closed on lost lease" response, it will need its own pin.

---

## Regression sweep

- `git diff 9d314a2 61734df -- src tests | grep '^-.*assert'` → **empty**. No assertion removed.
- Whole diff is **+1553 / −7**; the 7 deletions are one doc-comment and the retargeted test's
  setup lines (single shared store → two separate stores). No existing test weakened.
- ~15 advisory-default `GraphStore` impls compile and behave unchanged (default always-grants,
  persists nothing): both full test suites green (598 / 641), no churn to flush/canon/hybrid test
  stores.
- Same-process double-open advisory log still fires (`a_second_handle_on_one_session_is_reported
  _loudly` green).
- `#[ignore]` conventions intact: 1 ignored in each suite (the live-cockroach lease test).

## Could-not-reproduce / honest gaps

- **CockroachDB behaviour (T86-3) is code-read only** — no cluster (`LAMBO_COCKROACH_DSN` unset).
  The 40001-vs-auto-retry question is reasoned, not observed; the live conformance test
  (`single_writer_lease_is_enforced_across_pools`) is `#[ignore]`d for the same reason.
- **T86-1 frequency** measured at ~8% (1/12) on this machine under maximal-overlap launches; the
  real-world rate depends on OS scheduling and store latency and was not characterised beyond
  "reproducible."
- The 80-trial hammer used `sqlite3` to apply `migrations/sqlite/001_init.sql` because
  **`lambo provision` is a stub** ("use scripts/provision.sh for now") — worth noting on its own:
  the documented per-store bootstrap path for the lease table is not yet wired into the binary,
  though `SqliteStore::init_schema` and `scripts/provision.sh` both create it.
  **R2 note:** left as-is on purpose — the in-binary `provision` command is T8.3's scope
  (`docs(T8.3): expand scope to read+write parity`), out of T8.6. Tracked to **T8.3**, not
  fixed here.

_Scratchpad harnesses: `scratchpad/t86-review/{race.sh,race_verify.sh}` (build artifacts cleaned)._
```

---

## R2 Remediation disposition (CLOSED 2026-08-14)

Remediated on `task/t8.6-lease` (continued), 6 commits over the review commit `b0c164c`.
One commit per finding-group, each citing its finding id.

| Finding | Sev | Disposition | Commit |
|---|---|---|---|
| T86-2 lost-lease keeps writing | P2 | **FIXED** — heartbeat latches a fence; writes + flush + close fail closed | `d162814` |
| T86-1 loser gets opaque BUSY | P2 | **FIXED** — acquire the lease before `load_session` (reorder) | `f40f415` |
| T86-3 cockroach acquire not in `tx_retry` | P2 | **FIXED** — wrapped in `tx_retry` like every sibling write | `04bc8d0` |
| T86-6 winner-liveness untested | P3 | **FIXED** — cross-process test pins the winner live/holding + release | `29b2b12` |
| T86-4 unbounded non-serve close vs TTL | P3 | **DOCUMENTED** — caller's bounding contract on `close()` (smaller-risk) | `b4f1a6b` |
| T86-5 leaked-handle wedge | P3 | **DOCUMENTED** — accepted residual, operator-override escape | `b4f1a6b` |

**New tests (scorecard gap closed):**
- `a_fenced_loop_stops_flushing_and_drops_pending` (`src/store/flush.rs`) — once fenced, the
  write-behind loop stops and DROPS its pending; never flushes (no overwrite of the new holder).
- `a_lost_lease_fences_the_writer_and_stops_the_flush` (`src/memory.rs`) — after a real
  store-level takeover + fence, `derive`/`record_action`/`reserve` and `close()` are all refused
  with the honest lease-lost message, the tail is never persisted, and the takeover holder's
  lease stays intact (the fenced close does not release it).
- `a_second_process_on_one_session_is_refused_by_the_lease` (`tests/serve_single_writer_lease.rs`)
  extended to assert the WINNER serves through contention, exits 0 on graceful SIGTERM, and its
  lease is reacquirable afterward.

**Reorder-vs-translate choice (T86-1):** reordered (acquire before load), the reviewer's
preferred option — it is clean because the two operations are independent, and it makes *every*
loser's refusal the lease's honest message rather than translating one specific SQLite error
string. 20-trial repro: 0 `database is locked`, safety intact.

**Reorder-vs-document choice (T86-4):** documented. Keeping the heartbeat alive across the flush
would entangle the new T86-2 fence with the mid-close release/flush ordering, and the close body's
ordering is load-bearing for the R2-1/R3-1 cancellation + custody invariants (the T8.1 review
shows how reordering close repeatedly reopened P2s). The call-site bounding contract is the
smaller-risk option, and `serve` — the only in-repo production caller — already proves the window
closed via `const _` assertions.

**Gates (final, remediated tree @ `b4f1a6b`):** `cargo fmt --all -- --check` clean ·
`cargo clippy --all-targets -- -D warnings` clean · same `--features
store-cockroach,store-memory,fixtures` clean · same `--features store-sqlite` clean ·
`cargo test` **600 lib passed / 1 ignored** (+2 over the 598 baseline; zero removals) ·
`cargo test --features store-sqlite` **643 lib passed / 1 ignored** (+2 over 641; all
integration incl. `serve_single_writer_lease` green). No existing test weakened or deleted
(`grep '^-.*assert'` over the remediation diff is empty for pre-existing asserts).
