# Adversarial Review: P3 — Stores Tier — gemini36flash

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — dispositions recorded                   ║
║  Reviewer: gemini36flash (partial review, revised draft)  ║
║  Source: main @ 71a8dc9 (was adve-review-p3-partial.md)   ║
║  Date: 2026-08-11                                        ║
║  Scope: T3.1 (DDL), T3.4 (flush), T3.5 (load_session);   ║
║         T3.2/T3.3/T3.6 pending at review time             ║
║  Verdict: CONDITIONAL ACCEPT -> CLOSED                    ║
╚══════════════════════════════════════════════════════════╝
```

This is the **revised** gemini36flash draft (originally committed on `main` as
`adve-review-p3-partial.md`). The original opus46 draft is closed separately in
`adve-review-p3-stores-opus46.md`. Both shared one remediation pass
(`RemedP3Partial`, commit pending) because their findings are non-overlapping.

## Dispositions

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| M1 | MUST | `load_session` sync-over-async thread bridge risks reactor detachment under sqlx; spawns an OS thread per load | **FIXED** — async entry `load_session_async` added (async core; sync `load_session` becomes the thread-bridge wrapper); async callers (serve/daemon) `.await` directly on the active runtime (RemedP3Partial F4) |
| S1 | SHOULD | Retained failing batches cause 1-second retry log storms before degradation (3 retries + warn every tick until `log_max`) | **FIXED** — retained batch backs off after exhausted retries (no re-attempt every tick); degrade path unchanged (RemedP3Partial F3) |
| S2 | SHOULD | `reservations` DDL lacks reader guidance for expired soft locks — SQL readers see expired locks without `WHERE expires_at > now()` | **DOCUMENTED** — reader-filter comment added to the reservations table in both migration dialects (RemedP3Partial F6) |
| I1 | INFO | Timestamp ISO-8601 precision contract across dialects | **DOCUMENTED** — already satisfied: T3.3 (and T3.2) use `SecondsFormat::Millis` (24-char fixed), verified in the adapter reviews |

**Note (already satisfied by later work):** M1's reactor-detachment concern was
scrutinized in the T3.5/T3.3 reviews — the lazy-pool design (pools connect on
first async use) plus the documented cross-runtime quirk make the bridge sound
for production multi-thread runtimes; the async entry removes the concern for
async callers entirely.

## Original findings (gemini36flash draft)

Preserved below verbatim.

---

# Partial Adversarial Review: P3 — Stores Tier (In-Progress)

```yaml
reviewer:    adversarial-review-agent-opus46
date:        2026-08-11
phase:       P3
tasks_reviewed: T3.1 (Schema DDL), T3.4 (Flush Task), T3.5 (Load Session)
tasks_pending:  T3.2 (CockroachStore), T3.3 (SqliteStore), T3.6 (Structural Queries)
verdict:     CONDITIONAL ACCEPT — 1 must-fix (-opus46), 2 should-fix (-opus46), 1 informational (-opus46)
```

## 1. Executive Summary & Status Assessment

- **Completed & Landed**:
  - `T3.1` (Schema DDL for CockroachDB & SQLite with partial UNIQUE errata `concepts_key_non_obs_idx`).
  - `T3.4` (Write-behind `FlushTask` with interval/max_batch triggers & retry backoff).
  - `T3.5` (`load_session` materializing `LoadedSession` from `GraphStore`).
- **In Progress / Pending**:
  - `T3.2` (`CockroachStore` implementation over `sqlx::PgPool`).
  - `T3.3` (`SqliteStore` implementation over `sqlx::SqlitePool`).
  - `T3.6` (Structural SQL queries for `blast_radius` and `interaction_span`).

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

#### S2-opus46: `reservations` table DDL lacks index filtering for expired soft locks

**Location:** [`migrations/cockroach/001_init.sql:89-95`](file:///home/nryn/work/lambo/migrations/cockroach/001_init.sql#L89-L95) / [`migrations/sqlite/001_init.sql:107-113`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql#L107-L113)

**Description:**
`reservations` table stores advisory soft locks (`expires_at`). In SQL, expired reservations persist in the table until overwritten or explicitly deleted. Direct SQL readers (e.g., CockroachDB Cloud MCP server) querying `reservations` will see expired soft locks unless filtering explicitly by `WHERE expires_at > now()`.

**Fix (-opus46):**
Add explicit documentation in DDL migration comments specifying that external SQL reader queries against `reservations` must filter `WHERE expires_at > now()` (or `strftime('%Y-%m-%dT%H:%M:%fZ','now')`).

### INFORMATIONAL

#### I1-opus46: Timestamp ISO-8601 formatting precision contract across dialects

**Location:** [`migrations/sqlite/001_init.sql:18-23`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql#L18-L23)

**Notes:**
`T3.1` established the strict ISO-8601 UTC string format `YYYY-MM-DDTHH:MM:SS.SSSZ` (24 chars) for SQLite string timestamps. Ensure `T3.3` (`SqliteStore`) uses `chrono::SecondsFormat::Millis` consistently so lexicographical SQL comparisons (`WHERE created_at > ?`) match true chronological ordering.

## 3. Summary Verdict

**CONDITIONAL ACCEPT (-opus46)**. `T3.1`, `T3.4`, and `T3.5` are cleanly implemented and pass unit tests. Addressing `M1-opus46` (async entry point for `load_session`) will ensure zero connection driver issues when `T3.2` (`CockroachStore`) integrates.
