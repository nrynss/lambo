# Adversarial Review: P3 — Stores Tier — gemini36flash

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: OPEN — findings require remediation + review    ║
║  Reviewer: gemini36flash (partial review)                    ║
║  Date: 2026-08-11                                       ║
║  Disposition: PENDING — shared remediation pass         ║
║               (RemedP3Partial) + reviewer verification  ║
║               required before this review closes         ║
╚══════════════════════════════════════════════════════════╝
```

Do not close until the shared remediation (RemedP3Partial) lands and a
reviewer verifies the fixes. Dispositions are recorded on completion.

---

# Partial Adversarial Review: P3 — Stores Tier (In-Progress)

```yaml
reviewer:    adversarial-review-agent (-opus46)
date:        2026-08-11
phase:       P3
tasks_reviewed: T3.1 (Schema DDL), T3.4 (Flush Task), T3.5 (Load Session)
tasks_pending:  T3.2 (CockroachStore), T3.3 (SqliteStore), T3.6 (Structural Queries)
verdict:     CONDITIONAL ACCEPT — 1 must-fix (-opus46), 2 should-fix (-opus46), 1 informational (-opus46)
```

---

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

---

### SHOULD-FIX

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

---

### INFORMATIONAL

#### I1-opus46: Timestamp ISO-8601 formatting precision contract across dialects

**Location:** [`migrations/sqlite/001_init.sql:18-23`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql#L18-L23)

**Notes:**
`T3.1` established the strict ISO-8601 UTC string format `YYYY-MM-DDTHH:MM:SS.SSSZ` (24 chars) for SQLite string timestamps. Ensure `T3.3` (`SqliteStore`) uses `chrono::SecondsFormat::Millis` consistently so lexicographical SQL comparisons (`WHERE created_at > ?`) match true chronological ordering.

---

## 3. Summary Verdict

**CONDITIONAL ACCEPT (-opus46)**. `T3.1`, `T3.4`, and `T3.5` are cleanly implemented and pass unit tests. Addressing `M1-opus46` (async entry point for `load_session`) will ensure zero connection driver issues when `T3.2` (`CockroachStore`) integrates.
