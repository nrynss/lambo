# Adversarial Review (R2): T8.6 — Single-writer lease, re-verified at HEAD post-L82 + message-rewrite

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: RE-VERIFY at current HEAD (phase/p8-surface @ 0b4a0d8)      ║
║  Verdict: CLEAN — zero P1/P2. The L82 "lease release" change and      ║
║    the P3 lease-conflict message rewrite did NOT regress the          ║
║    store-enforced single-writer safety property or the done-when.     ║
║  Central claim: exactly one holder / one honest refusal, atomic       ║
║    acquire per backend, fail-closed naming holder+age, heartbeat +    ║
║    expiry, release-on-close, serve acquires on start + releases on    ║
║    every exit path, readers never touch the lease.                    ║
║  P1: none · P2: none · P3: 2 (records, neither actionable in-repo)    ║
║  Cockroach live leg: INFRA-BLOCKED (no LAMBO_COCKROACH_DSN) —         ║
║    compile-only + reviewable, recorded, NOT claimed.                  ║
║  Opened: 2026-08-15                                                    ║
╚══════════════════════════════════════════════════════════════════════╝
```

**Task:** T8.6 — Single-writer lease, store-enforced (spec §2.2).
**Branch reviewed:** `phase/p8-surface` @ `0b4a0d8` (clean tree, `git status` clean).
**Why re-review:** prior T8.6 verdict was R2-VERIFY CLEAN at `61734df`. Since then the branch
merged `task/live-l82-remediation` (`8134a3c`, incl. `060cca1` — the L82-1 fix: bulk the flush
write path + **release the lease on an abandoned close**) and a P3 message remediation
(`204f340`, T88-H6) that rewrote the lease-conflict error MESSAGE in `src/memory.rs`. This
review re-verifies that neither regressed the T8.6 safety property or its done-when.
**Scope:** `src/store/lease.rs`, lease methods in `src/store/{mod,memory,sqlite,cockroach}.rs`,
`build/close/heartbeat/drop` wiring in `src/memory.rs`, serve lifecycle in `src/mcp/serve.rs`,
`migrations/{sqlite,cockroach}/001_init.sql`.
**Method:** clause-by-clause read of every lease surface at HEAD; the full PHASE-8 binding gate
block run independently; targeted execution of the cross-process lease battle-tests
(`serve_single_writer_lease`, `cli_write_lease`) and the message-pinned memory test. No source
mutation performed — the reviewer is explicitly forbidden from editing files, so the atomicity/
release pinning is established by (a) code unchanged in the critical regions vs. the prior
mutation-verified review and (b) the green test suite, not by a fresh mutation.

---

## Change-under-review (what could have regressed)

1. **L82-1 lease release (`060cca1`).** New `Memory::release_lease_after_abandoned_close` +
   `serve::release_lease_bounded`, so a `close()` abandoned by `serve` (deadline blow or a second
   signal) still releases the lease instead of leaving a stale row wedging the session for a TTL.
   Also added the bulk-flush planner (`store/batch.rs`) and rewrote the SQLite/Cockroach flush +
   seed write paths. **Verified: this is an addition, not a removal — it strictly widens
   release-on-exit coverage.**
2. **P3 message rewrite (`204f340`).** The `Memory::build` refusal dropped the inline Spec §2.2
   citation and the raw `OPERATOR_OVERRIDE` SQL from the user-facing string, now pointing at
   `docs/reference/cli.mdx`. Still returns `LamboError::Conflict`, still names holder + age,
   still fails closed. Pinned test updated to the new contract.

## Method — what was verified live vs. read

**Gates (run independently on the clean tree at HEAD):**

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | clean (exit 0) |
| `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings` | clean (exit 0) |
| `cargo clippy --all-targets --features store-sqlite -- -D warnings` | clean (exit 0) |
| `cargo test` | **710 lib passed / 1 ignored** (live BGE llama server), 5 bin, all integration green |
| `cargo test --features store-sqlite` | **757 lib passed / 1 ignored**, 5 bin, all integration green |
| `cargo test --no-default-features --features store-sqlite --no-run` | compile clean (exit 0) |
| `cargo test --no-default-features --features store-cockroach --no-run` | compile clean (exit 0) |
| `cargo check --no-default-features` | clean (exit 0) |

**Cross-process battle-tests (executed):**
- `tests/serve_single_writer_lease.rs::a_second_process_on_one_session_is_refused_by_the_lease` —
  **ok** (winner serves through contention, loser fails closed, reacquire after winner exits).
- `tests/cli_write_lease.rs::derive_succeeds_with_no_serve_and_fails_closed_while_serve_holds` —
  **ok** (write succeeds with no holder; fails closed naming the holder while `serve` owns it).
- `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease` (the message-pinned
  test) — **ok**, still asserts `Conflict`, holder `agent-a`, `"s ago"` age, the
  `"operator can force a takeover"` path, and `docs/reference/cli.mdx`.

---

## Re-verification of each T8.6 property

### 1. Atomic acquisition per backend — INTACT
- **MemoryStore** (`store/memory.rs:312-360`): one `leases.write()` map lock held for the whole
  decision — the in-RAM analogue of the SQL upsert; no reader can observe a half-applied steal.
- **SqliteStore** (`store/sqlite.rs:977-1035`): ONE `INSERT … ON CONFLICT (session_id) DO UPDATE …
  WHERE expires_at <= strftime('now') OR holder = excluded.holder … RETURNING`, executed under
  SQLite's write lock with no read-then-write race. `session_id` is `PRIMARY KEY`;
  `strftime(...,'now')` is the store clock, never a client value (F18).
- **CockroachStore** (`store/cockroach.rs:1316-1394`): same single-statement upsert, timestamps
  from `now()`; the body is wrapped in `tx_retry` (T86-3 fix, `:1347`) so a SQLSTATE 40001 replays
  transparently. **Compile-only below** — the live 40001-vs-auto-retry question remains unobserved
  (no cluster), unchanged from R1/R2.
- Neither L82 (<sub>bulk-flush refactor lived in the same files</sub>) nor the message rewrite
  touched the ACQUIRE SQL or the MEMORY map lock — both are unchanged from the prior
  mutation-verified revision.

### 2. Fail-closed naming holder+age — INTACT (message rewrite is cosmetic-only)
`Memory::build` (`memory.rs:550-567`) STILL returns `Err(LamboError::Conflict)` on
`LeaseOutcome::Held`, formatting `"session {s} is already held by another writer
({holder}) — it acquired the single-writer lease {age}s ago and is still refreshing it. Refusing
to open a second writer. If that holder is wedged, an operator can force a takeover (see the
single-writer lease note in docs/reference/cli.mdx)"`. The refusal class, the holder identity,
and the age are all preserved. The raw `session_leases` SQL constant (`lease::OPERATOR_OVERRIDE`)
is no longer in the user-facing string but remains in the module docs + both migration comments
for operators who know the store. The pinned test passes. **No safety regression.**

### 3. Heartbeat refresh + expiry-after-crash + release-on-close — INTACT, release WIDENED
- Heartbeat (`memory.rs:328-370`): `spawn_lease_heartbeat` refreshes at `LEASE_HEARTBEAT_INTERVAL`
  (TTL/3); a `Held` refresh latches the shared T86-2 `fence` (writer refuses, flush drops its
  tail); a transient error logs and keeps beating. Unchanged.
- Expiry-after-crash: `Drop` aborts the heartbeat (`memory.rs:1757-1761`) so a dropped/crashed
  handle's lease lapses at TTL — unchanged.
- Release-on-close: `release_lease_once` fires on every **success** path of `close()` (empty-log
  shortcut `:1643`, completed final flush `:1668`), guarded by `lease_released` so a retried close
  does not release twice; a **failed** flush still deliberately does NOT release (keeps the lease
  for retry, lapses at TTL) — all unchanged.
- **L82 addition (verified, not a regression):** `release_lease_after_abandoned_close`
  (`memory.rs:1820-1834`) releases even when `serve` *abandons* `close()` (deadline / second
  signal), aborting the heartbeat first and skipping when fenced. This closes the stale-lease
  wedge the live review (L82-1) found. Read this as strictly-more release coverage, consistent
  with "graceful close releases; crash lapses at TTL."

### 4. `serve` acquires on start and releases on EVERY exit path — INTACT
- Acquire: `serve::build_memory` → `Memory::build` acquires at step 0 (`memory.rs:550`), before
  the startup load (T86-1).
- Release per exit path (`serve.rs:735-865`):
  - clean transport exit → `close_bounded` → `Memory::close` succeeds → `release_lease_once`
    (empty-tail `:1643` / post-flush `:1668`);
  - `close()` deadline blow (`:809-822`) → `release_lease_bounded` → abandoned-close release;
  - second shutdown signal during close (`:824-834`) → `release_lease_bounded` → abandoned-close
    release;
  - a fenced handle → refuses and does NOT release (lease belongs to the new holder) — correct.
  `close_bounded_until` runs the release whenever `outcome.is_err()` (`:838-841`). Release window
  (`LEASE_RELEASE_GRACE`) is carved OUT of `CLOSE_GRACE` (not added), so `SHUTDOWN_BUDGET` and the
  `LEASE_TTL > SHUTDOWN_BUDGET` invariant are unchanged (both `const _` pins still present,
  `serve.rs:91-139`). Lifecycle tests green: `a_clean_close_releases_the_lease`,
  `an_abandoned_close_releases_the_lease_through_serve`, `a_close_that_finishes_releases_exactly_once_through_serve`,
  `a_fenced_handle_does_not_release_on_an_abandoned_close`.

### 5. Readers never touch the lease — INTACT
CLI read verbs (`recall`, `saints`, `inspect`, `stats`) build stores for reading and call no
`acquire_lease`; only writer-mode `Memory::build` acquires. The sole `.build()` in
`src/cli/saints.rs` is inside a `#[cfg(test)]` writer parity test, not the read path. The L82 and
message changes did not touch the read verbs.

### 6. Test suite for the done-when — HOLDS and RUNS
`tests/serve_single_writer_lease.rs` (cross-process, sqlite) and `tests/cli_write_lease.rs` both
**pass** under `--features store-sqlite` (executed above). The memory/sqlite in-process lease
tests (acquire/Held/refresh/release, expiry-after-crash, holder-scoped release, cross-connection
serialize, shared-store refusal) are all green in the 757-lib suite. **Pin strength:** the
critical predicates (atomic acquire guard, release-on-close) are byte-identical to the region
the previous review mutation-proved (scorecard mutations a/b/c). No fresh mutation run here —
forbidden to edit files; the unchanged-code + green-suite evidence is dispositive for the
re-review claim.

### 7. Cockroach leg — COMPILE-ONLY + INFRA-BLOCKED (recorded, not claimed)
- `LAMBO_COCKROACH_DSN` is **unset**; the cockroach CI row is `--no-run` (compile-only). It
  **compiles clippy-clean** under `--features store-cockroach,store-memory,fixtures -D warnings`
  and the `--no-default-features --features store-cockroach --no-run` row — the atomic acquire
  (`cockroach.rs:1316-1394`), the `tx_retry` wrap (T86-3), and holder-scoped release
  (`cockroach.rs:1795-1810`) are all reviewable and present.
- The live cross-pool done-when test `single_writer_lease_is_enforced_across_pools`
  (`cockroach.rs:3073`) is `#[ignore = "live: requires LAMBO_COCKROACH_DSN"]` with an honest
  `dsn_or_skip` gate and a non-ignored `live_dsn_gate_fails_loudly_when_required` honesty check —
  it reports ignored, never skip-as-green.
- **Explicit:** the live cockroach cross-process single-writer done-when leg is **unperformed and
  infra-blocked** here (same as T8.4/T8.5). It is NOT claimed as verified. A cluster holder must
  run the `-- --ignored` conformance suite to close it.

---

## Findings

### T86R2-1 (P3, record — no action) — message rewrite removed the inline takeover SQL from the refusal
- **Where:** `src/memory.rs:559-566` (post-`204f340`); `src/store/lease.rs:97`
  (`OPERATOR_OVERRIDE` retained in docs/migrations only).
- **What:** The user-facing refusal no longer prints the literal
  `DELETE FROM session_leases WHERE session_id = '<session>';` takeover statement; it now points
  at `docs/reference/cli.mdx`. Safety is intact (still `Conflict`, names holder+age, fails
  closed). The only cost is operator convenience — an operator who previously copy-pasted the SQL
  now must open the doc. This was an intentional, already-tracked P3 (T88-H6) with its pinned test
  updated; it is not a regression of any T8.6 property. Recorded here so the T8.6 record of the
  refusal message reflects the current contract (the prior review quoted the old string).

### T86R2-2 (P3, record — infra) — live Cockroach cross-process done-when leg remains unperformed
- **Where:** `src/store/cockroach.rs:3073` (`single_writer_lease_is_enforced_across_pools`).
- **What:** No live cluster / `LAMBO_COCKROACH_DSN` here, so the cross-pool acquire-enforce + expiry
  leg of the "Done when" is compile-only and unobserved — identical to T8.4/T8.5's infra-blocked
  legs. The 40001-auto-retry-vs-tx_retry question from T86-3 stays reasoned-not-observed. A cluster
  holder must run the `-- --ignored` conformance suite before the phase-wide exit criteria can
  claim the cockroach single-writer line.

---

## Verified-OK (probed, not defects)

- **Atomicity / clock discipline (F18)** — unchanged: all three backends stamp from their own
  clock (`now()` / `strftime('now')` / `Utc::now()`), never a client arg; no wire-visible lease
  field added by L82 or the message change.
- **Bounded `Held` retry loop** — `for 0..3` in the SQL backends retries only the vanished-row
  case, returns a typed `Backend` error after exhaustion. Unchanged.
- **Fence (T86-2) still intact** — `close()`'s fenced branch (`memory.rs:1541-1556`) refuses to
  flush/release a lost lease; `a_lost_lease_fences_the_writer_and_stops_the_flush` green.
- **Serving through contention + winner liveness** (T86-6) — the cross-process test still asserts
  A serves via `tools/list` after B's refusal; green.
- **`LEASE_TTL (45s) > SHUTDOWN_BUDGET (15s)`** and **`CLOSE_FLUSH_GRACE + LEASE_RELEASE_GRACE ==
  CLOSE_GRACE`** — both `const _` pins present and compiling.
- **Outstanding prior residuals remain documented, none re-opened:** the detection-latency
  fencing window (tracked `nrynss/lambo#1`, out of T8.6 scope), the non-serve unbounded
  `close()`-vs-TTL contract (T86-4), and the leaked-handle wedge (T86-5).

---

## Scorecard

| Property | Re-verified | Evidence |
|---|---|---|
| Atomic acquire (all 3 backends) | ✅ INTACT | code read (memory map-lock; sqlite/cockroach single-statement upsert) |
| Fail-closed naming holder+age | ✅ INTACT | message-pinned test green; `Conflict` + holder + age preserved |
| Heartbeat / expiry-after-crash | ✅ INTACT | `spawn_lease_heartbeat` + `Drop` abort unchanged |
| Release-on-close (every success path) + L82 abandoned-close release | ✅ INTACT (widened) | `close()` + L82 `release_lease_after_abandoned_close`; lifecycle tests green |
| serve acquires start / releases every exit | ✅ INTACT | `serve.rs:735-865`; 4 lifecycle release tests green |
| Readers never touch the lease | ✅ INTACT | read verbs lease-free; writer-only acquire |
| Done-when cross-process tests | ✅ HOLDS | `serve_single_writer_lease` + `cli_write_lease` pass |
| Cockroach leg | ⚠️ compile-only / INFRA-BLOCKED | clippy-clean; conformance test honest-ignored; DSN unset — NOT claimed |

**Regression check:** `git diff 61734df..HEAD -- src/store/lease.rs` is +10 (docs only); the
ACQUIRE SQL, the map-lock acquire, `release_lease_once`, the fence, and the heartbeat are all
unchanged from the mutation-verified revision. The only lease-code delta is the *added*
abandoned-close release and the *edited* refusal string — both verified above to preserve the
safety property.

---

## Verdict

**CLEAN.** The L82 lease-release change is an additive widening of release-on-exit coverage, not
a regression; the P3 message rewrite preserves fail-closed, holder+age naming, and the `Conflict`
class. No P1, no P2, no new actionable defect. The Cockroach cross-process done-when leg is
unperformed and infra-blocked — recorded, not claimed, pending a cluster holder.

---

## SUPERSEDED (2026-08-15) — T86R2-2 CLOSED by the live conformance run

The "INFRA-BLOCKED / NOT claimed / unperformed" record above for **T86R2-2** is superseded:
the live CockroachDB Cloud conformance suite
(`scripts/run-live-cockroach.sh`, `LAMBO_REQUIRE_LIVE=1`) ran **8/8 green**, including
`single_writer_lease_is_enforced_across_pools` (and `cockroach_three_hop_progression_matches_memory`,
`saints_and_stats_on_live`, and the vector `EXPLAIN` camera proof). The Cockroach cross-pool
done-when leg is therefore **observed and enforced**, not compile-only. Evidence:
`evidence/demo-live-conformance.txt`. Recorded in PHASE-8 Handoff "T86R2-2 CLOSED
(2026-08-15)". The historical INFRA-BLOCKED verdict body above is left as written.
