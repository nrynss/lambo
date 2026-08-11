# Partial Adversarial Review: P3 — Stores Tier (In-Progress)

```yaml
<<<<<<< HEAD
reviewer:    adversarial-review-agent (-opus46)
date:        2026-08-11
phase:       P3
tasks_reviewed: T3.1 (Schema DDL), T3.4 (Flush Task), T3.5 (Load Session)
tasks_pending:  T3.2 (CockroachStore), T3.3 (SqliteStore), T3.6 (Structural Queries)
verdict:     CONDITIONAL ACCEPT — 1 must-fix (-opus46), 2 should-fix (-opus46), 1 informational (-opus46)
=======
reviewer:    adversarial-review-agent-opus46
date:        2026-08-11
phase:       P3
branch:      phase/p3-stores
tasks_done:  T3.1 (Schema DDL), T3.4 (Flush Task), T3.5 (Load Session)
tasks_open:  T3.2 (CockroachStore), T3.3 (SqliteStore), T3.6 (Structural Queries)
verdict:     CONDITIONAL ACCEPT — 1 must-fix, 3 should-fix, 3 informational
>>>>>>> df04aa8 (docs: add partial adversarial review for P3 (-opus46))
```

---

<<<<<<< HEAD
## 1. Executive Summary & Status Assessment

- **Completed & Landed**:
  - `T3.1` (Schema DDL for CockroachDB & SQLite with partial UNIQUE errata `concepts_key_non_obs_idx`).
  - `T3.4` (Write-behind `FlushTask` with interval/max_batch triggers & retry backoff).
  - `T3.5` (`load_session` materializing `LoadedSession` from `GraphStore`).
- **In Progress / Pending**:
  - `T3.2` (`CockroachStore` implementation over `sqlx::PgPool`).
  - `T3.3` (`SqliteStore` implementation over `sqlx::SqlitePool`).
  - `T3.6` (Structural SQL queries for `blast_radius` and `interaction_span`).

---

## 2. Partial Review Findings

### MUST-FIX

#### M1-opus46: `load_session` sync-over-async thread bridge risks reactor detachment under `sqlx`

**Location:** [`src/store/load.rs:115-130`](file:///home/nryn/work/lambo/src/store/load.rs#L115-L130)

**Description:**
`load_session` bridges the sync `load_session` API to the async `GraphStore::load_session` by spawning a new `std::thread` with a private single-threaded Tokio runtime:
```rust
std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(store.load_session(session_id))
})
```

**Risk:**
When `store` is a `CockroachStore` or `SqliteStore` holding an `sqlx::Pool`, the underlying pool connections are bound to the caller's main Tokio reactor/driver. Running `store.load_session` inside a fresh, isolated single-threaded runtime on a temporary OS thread can lead to `sqlx` connection driver detachments, timer panics, or `reactor gone` errors during I/O. Furthermore, spawning an OS thread per session load creates unnecessary thread allocation overhead.

**Fix (-opus46):**
Provide an `async fn load_session_async` entry point for async callers (such as `lambo serve` and daemon tasks) so they can `.await` directly on the active Tokio runtime. Reserve the thread-bridge fallback strictly for non-async CLI callers.
=======
## Scope

This partial review covers the three completed P3 tasks against the frozen
spec and the P2 integration contracts. Code examined:
[`src/store/mod.rs`](file:///home/nryn/work/lambo/src/store/mod.rs) (20 KB),
[`src/store/memory.rs`](file:///home/nryn/work/lambo/src/store/memory.rs) (32 KB),
[`src/store/flush.rs`](file:///home/nryn/work/lambo/src/store/flush.rs) (49 KB),
[`src/store/load.rs`](file:///home/nryn/work/lambo/src/store/load.rs) (19 KB),
[`migrations/cockroach/001_init.sql`](file:///home/nryn/work/lambo/migrations/cockroach/001_init.sql),
[`migrations/sqlite/001_init.sql`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql).

---

## Findings

### MUST-FIX

#### M1-opus46: `FlushTask` retry replays the full batch — SQL adapters with PK constraints will fail on partial-then-retry

**Location:** [`flush.rs` flush_with_retry / cycle](file:///home/nryn/work/lambo/src/store/flush.rs#L240-L290)

When `store.flush(&batch)` errors or panics mid-execution (after some
mutations have already been committed to the database), `FlushTask` retains
the **entire** `self.pending` batch and replays it on the next attempt.

For `MemoryStore` this is harmless (in-memory upserts are idempotent). For
`CockroachStore` and `SqliteStore` (T3.2/T3.3), replaying already-committed
`UpsertNode` mutations will hit `PRIMARY KEY` or `UNIQUE` constraint
violations unless the SQL uses `INSERT ... ON CONFLICT DO UPDATE` (upsert
semantics) for every mutation kind.

**Risk:** T3.2/T3.3 must implement every `flush()` mutation as an idempotent
upsert, not a plain `INSERT`. If any mutation kind uses bare `INSERT`, a
partial-failure retry will hard-error and the batch will never land — causing
eventual degradation.

**Fix:** Either:
- (a) **Document the idempotency contract** on `GraphStore::flush`: "Adapters
  MUST use upsert/ON CONFLICT semantics for all mutation kinds. The flush task
  may replay a batch that partially succeeded." Add this to the trait docstring
  and the P3 phase doc's T3.2/T3.3 guidance. This is the recommended fix.
- (b) Track a high-water mark in the batch so retries skip already-applied
  mutations. This is more complex and unnecessary if (a) is followed.
>>>>>>> df04aa8 (docs: add partial adversarial review for P3 (-opus46))

---

### SHOULD-FIX

<<<<<<< HEAD
#### S1-opus46: Retained failing batches cause 1-second retry log storms before degradation

**Location:** [`src/store/flush.rs:210-245`](file:///home/nryn/work/lambo/src/store/flush.rs#L210-L245)

**Description:**
When a flush batch encounters an unrecoverable failure (e.g. database schema constraint violation), `FlushTask` executes `backend_flush_retries` (3) exponential backoff retries, emits `warn!("BackendFlushFailed")`, and retains the batch in `self.pending`.

However, on the very next 1-second interval tick, `FlushTask` immediately attempts to flush `self.pending` again, repeating the 3 retries and log warnings every single second until `backend_log_max` (50,000) is reached.

**Impact:**
Spams warning logs 60 times per minute with 180 failed DB attempts per minute while waiting for the log depth cap to trigger `durability="none"`.

**Fix (-opus46):**
Implement backoff multiplier on the interval loop tick when `self.pending` contains a previously failed batch, or increase interval sleep temporarily after exhausted retries.

---

#### S2-opus46: `reservations` table DDL lacks index filtering for expired soft locks

**Location:** [`migrations/cockroach/001_init.sql:89-95`](file:///home/nryn/work/lambo/migrations/cockroach/001_init.sql#L89-L95) / [`migrations/sqlite/001_init.sql:107-113`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql#L107-L113)

**Description:**
`reservations` table stores advisory soft locks (`expires_at`). In SQL, expired reservations persist in the table until overwritten or explicitly deleted. Direct SQL readers (e.g., CockroachDB Cloud MCP server) querying `reservations` will see expired soft locks unless filtering explicitly by `WHERE expires_at > now()`.

**Fix (-opus46):**
Add explicit documentation in DDL migration comments specifying that external SQL reader queries against `reservations` must filter `WHERE expires_at > now()` (or `strftime('%Y-%m-%dT%H:%M:%fZ','now')`).
=======
#### S1-opus46: No graceful shutdown drain — pending mutations silently lost on task abort

**Location:** [`flush.rs` FlushTask](file:///home/nryn/work/lambo/src/store/flush.rs)

There is no shutdown signal, `Drop` impl, or final-flush mechanism. If the
`JoinHandle` returned by `spawn()` is dropped or aborted (e.g., process
shutdown, Ctrl-C), any mutations in `self.pending` or still in the graph's
mutation log are silently lost.

**Spec §2.4:** "the loss bound is *observable*, not assumed." The loss bound
is observable via `stats().depth`, but there is no mechanism to *act* on it
at shutdown.

**Risk:** For the v0.1 demo this is acceptable (sessions are short-lived,
the graph is the primary tier). For any production use, a graceful shutdown
that drains the final batch is expected.

**Fix:** Add a `CancellationToken` or `shutdown` channel. On signal, perform
one final drain+flush before the loop exits. Low priority for v0.1 — note
for v0.7.0.

---

#### S2-opus46: `interaction_span` returns `coverage: 0.0` for single-interaction sessions

**Location:** [`memory.rs` interaction_span](file:///home/nryn/work/lambo/src/store/memory.rs)

When a session has exactly one interaction, `sess_span = (sess_hi - sess_lo)`
equals `0.0`. The coverage calculation divides by `sess_span` and falls back
to `0.0`. This means a concept that spans the entirety of a single-interaction
session reports `coverage: 0.0` instead of `1.0`.

**Impact:** Canonization Stage 2 (spec §10) uses `interaction_span.coverage`
as structural evidence. A single-interaction session will never produce
non-zero coverage, potentially blocking canonization in short sessions.

**Fix:** When `sess_span <= 0.0` and `distinct >= 1`, return `coverage: 1.0`
(the concept covers 100% of the session's temporal extent, which is a single
point).

---

#### S3-opus46: `load_session` sync bridge has no timeout — hangs indefinitely on store deadlock

**Location:** [`load.rs` block_on helper](file:///home/nryn/work/lambo/src/store/load.rs#L98-L130)

The `std::thread::scope` + `rt.block_on(fut)` bridge will block the calling
thread indefinitely if the underlying `GraphStore::load_session` hangs (e.g.,
CockroachDB connection timeout, pool exhaustion). There is no
`tokio::time::timeout` wrapper.

**Risk:** During `lambo serve` startup, a hung store would freeze the process
with no error message.

**Fix:** Wrap the store call in `tokio::time::timeout(Duration::from_secs(30), store.load_session(...))` inside the private runtime. Map timeout to
`StoreError::Backend("load_session timed out after 30s")`.
>>>>>>> df04aa8 (docs: add partial adversarial review for P3 (-opus46))

---

### INFORMATIONAL

<<<<<<< HEAD
#### I1-opus46: Timestamp ISO-8601 formatting precision contract across dialects

**Location:** [`migrations/sqlite/001_init.sql:18-23`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql#L18-L23)

**Notes:**
`T3.1` established the strict ISO-8601 UTC string format `YYYY-MM-DDTHH:MM:SS.SSSZ` (24 chars) for SQLite string timestamps. Ensure `T3.3` (`SqliteStore`) uses `chrono::SecondsFormat::Millis` consistently so lexicographical SQL comparisons (`WHERE created_at > ?`) match true chronological ordering.

---

## 3. Summary Verdict

**CONDITIONAL ACCEPT (-opus46)**. `T3.1`, `T3.4`, and `T3.5` are cleanly implemented and pass unit tests. Addressing `M1-opus46` (async entry point for `load_session`) will ensure zero connection driver issues when `T3.2` (`CockroachStore`) integrates.
=======
#### I1-opus46: `Utc::now()` in `MemoryStore` structural queries makes test timing fragile

**Location:** [`memory.rs` blast_radius / interaction_span](file:///home/nryn/work/lambo/src/store/memory.rs)

Both `blast_radius` and `interaction_span` filter edges by
`created_at <= Utc::now() - min_edge_age`. Tests creating edges with
`Utc::now()` timestamps and then immediately querying with a non-zero
`min_age` may non-deterministically exclude edges that were created
microseconds too recently.

**Note:** The existing tests pass `Duration::ZERO` for `min_edge_age`, which
avoids this. T3.2/T3.3 SQL adapters should use the same pattern, or use
backdated fixture timestamps.

---

#### I2-opus46: `MemoryStore` delete resolution scans all sessions — O(sessions × snapshot_size)

**Location:** [`memory.rs` resolve_session_for_node / resolve_session_for_edge](file:///home/nryn/work/lambo/src/store/memory.rs)

When a `DeleteNode` or `DeleteEdge` mutation lacks an attached session ID,
`MemoryStore` scans every session's snapshot to find the owning session.

**Note:** Fine for test workloads (single session). SQL adapters will use
indexed queries and don't have this issue.

---

#### I3-opus46: Graph lock discipline is clean — verified no `.await` under lock

**Location:** [`flush.rs` lines 272-278, 384](file:///home/nryn/work/lambo/src/store/flush.rs)

Verified three lock acquisition sites in `FlushTask`:
1. Line 272: READ lock for `session_id()` clone — dropped immediately.
2. Lines 273-278: WRITE lock for `drain_log()` — scoped block, dropped before
   any store I/O.
3. Line 384: READ lock for `log_len()` in `refresh_depth()` — sync function,
   no `.await`.

All three comply with spec §6.4. No findings.

---

## Test Coverage Summary

| Module | Tests | Feature-gated | Notes |
|---|---|---|---|
| `mod.rs` (config/registry) | 14 | 0 | Level B selection, TOML parsing, fail-closed, DSN redaction |
| `memory.rs` (MemoryStore) | 11 | 0 | Round-trip, isolation, keyword search, blast radius, concurrent flush |
| `flush.rs` (FlushTask) | 8 | 0 | Interval/max-batch, retry/backoff, degradation, panic containment, ordering |
| `load.rs` (load_session) | 6 | 4 (`store-memory`) + 1 (`fixtures`) | Round-trip, missing session, corruption, sync-context |
| **Total** | **39** | | |

### Notable gap:
- `load.rs` has 5 of 6 tests behind feature gates. `cargo test` with default
  features runs only 1 of the 6 load tests. Acceptable (features are additive),
  but CI must run `--all-features` or `--features store-memory,fixtures`.

---

## Downstream Guidance for T3.2 / T3.3

| Contract | Source | What adapters must do |
|---|---|---|
| Idempotent flush | M1-opus46 | Every mutation kind must use `ON CONFLICT` upsert semantics |
| Chronological order | `drain_log` contract | Replay mutations in submission order, never reorder into §2.4 phases |
| `reinforcements = 1` on creation | P2 I2 | Match graph core convention, not DDL default of 0 |
| Partial UNIQUE | T3.1 DDL | `ON CONFLICT` must spell `WHERE concept_type <> 'Observation'` |
| Timestamp format | T3.1 handoff | `chrono::to_rfc3339_opts(SecondsFormat::Millis, true)` — 24-char `YYYY-MM-DDTHH:MM:SS.SSSZ` |
| `vector_dimensions()` | Trait default | CockroachStore: `Some(1024)` from schema; SqliteStore: `None` |

---

## Verdict

**CONDITIONAL ACCEPT.** The store tier infrastructure (trait, registry,
MemoryStore, flush task, session loader) is well-engineered with 39 tests,
clean lock discipline, and proper panic containment. M1-opus46 (idempotency
contract documentation) must be addressed before T3.2/T3.3 start writing
their `flush()` implementations — it's a documentation fix, not a code change.
S2-opus46 (single-interaction coverage) should be fixed before P6 canonization
consumes `interaction_span`.
>>>>>>> df04aa8 (docs: add partial adversarial review for P3 (-opus46))
