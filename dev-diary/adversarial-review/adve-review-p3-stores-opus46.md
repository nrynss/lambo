# Adversarial Review: P3 — Stores Tier — opus46

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — dispositions recorded                   ║
║  Reviewer: opus46 (partial review, original draft)        ║
║  Source: df04aa8 (phase lineage; was adve-review-p3-partial.md) ║
║  Date: 2026-08-11                                        ║
║  Scope: T3.1 (DDL), T3.4 (flush), T3.5 (load_session);   ║
║         T3.2/T3.3/T3.6 pending at review time             ║
║  Verdict: CONDITIONAL ACCEPT -> CLOSED                    ║
╚══════════════════════════════════════════════════════════╝
```

This is the **original** opus46 draft (committed as `df04aa8`). The revised
gemini36flash draft is closed separately in
`adve-review-p3-stores-gemini36flash.md`. Both shared one remediation pass
(`RemedP3Partial`, commit pending) because their findings are non-overlapping.

## Dispositions

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| M1 | MUST | Flush retry replays the full batch — SQL adapters with PK constraints will fail on partial-then-retry unless every mutation kind is an idempotent upsert | **FIXED + DOCUMENTED** — both adapters (T3.2/T3.3) already implement every mutation kind with `ON CONFLICT` upsert semantics (verified in their round reviews); the contract is now pinned on the `GraphStore::flush` trait docstring (RemedP3Partial F5) |
| S1 | SHOULD | No graceful shutdown drain — pending mutations silently lost on task abort | **DOCUMENTED** — deferred to v0.7.0 (acceptable for v0.1: sessions short-lived, graph is the primary tier); note in PHASE-3 handoff (RemedP3Partial handoff notes) |
| S2 | SHOULD | `interaction_span` returns `coverage: 0.0` for single-interaction sessions (sess_span = 0) — blocks canonization Stage 2 in short sessions | **FIXED** — coverage 1.0 when extent is a single point and distinct >= 1, in MemoryStore AND both SQL adapters (three-way parity kept); tests added (RemedP3Partial F1) |
| S3 | SHOULD | `load_session` sync bridge has no timeout — a hung store freezes startup indefinitely | **FIXED** — 30s `tokio::time::timeout` in the bridge, mapped to `StoreError::Backend`; parameterized helper + hanging-store test (RemedP3Partial F2) |
| I1 | INFO | `Utc::now()` in MemoryStore structural queries makes test timing fragile | **DOCUMENTED** — existing tests pass `Duration::ZERO` min-age; adapters use Rust-computed cutoffs (note in PHASE-3 handoff) |
| I2 | INFO | MemoryStore delete resolution scans all sessions (O(sessions × size)) | **DOCUMENTED** — fine for test workloads; SQL adapters use indexed queries (note in PHASE-3 handoff) |
| I3 | INFO | Graph lock discipline clean — verified no `.await` under lock | No action (positive verification, matches T3.4 review) |

**Downstream guidance table** (idempotent flush, chronological order,
`reinforcements = 1`, partial UNIQUE `ON CONFLICT WHERE`, timestamp format,
`vector_dimensions`) — all six were followed by T3.2/T3.3 and verified in their
round reviews; the flush idempotency contract is now pinned on the trait.

## Original findings (opus46 draft)

Preserved below verbatim.

---

# Partial Adversarial Review: P3 — Stores Tier (In-Progress)

```yaml
reviewer:    adversarial-review-agent-opus46
date:        2026-08-11
phase:       P3
branch:      phase/p3-stores
tasks_done:  T3.1 (Schema DDL), T3.4 (Flush Task), T3.5 (Load Session)
tasks_open:  T3.2 (CockroachStore), T3.3 (SqliteStore), T3.6 (Structural Queries)
verdict:     CONDITIONAL ACCEPT — 1 must-fix, 3 should-fix, 3 informational
```

## Scope

This partial review covers the three completed P3 tasks against the frozen
spec and the P2 integration contracts. Code examined:
[`src/store/mod.rs`](file:///home/nryn/work/lambo/src/store/mod.rs) (20 KB),
[`src/store/memory.rs`](file:///home/nryn/work/lambo/src/store/memory.rs) (32 KB),
[`src/store/flush.rs`](file:///home/nryn/work/lambo/src/store/flush.rs) (49 KB),
[`src/store/load.rs`](file:///home/nryn/work/lambo/src/store/load.rs) (19 KB),
[`migrations/cockroach/001_init.sql`](file:///home/nryn/work/lambo/migrations/cockroach/001_init.sql),
[`migrations/sqlite/001_init.sql`](file:///home/nryn/work/lambo/migrations/sqlite/001_init.sql).

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

### SHOULD-FIX

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

### INFORMATIONAL

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

#### I2-opus46: `MemoryStore` delete resolution scans all sessions — O(sessions × snapshot_size)

**Location:** [`memory.rs` resolve_session_for_node / resolve_session_for_edge](file:///home/nryn/work/lambo/src/store/memory.rs)

When a `DeleteNode` or `DeleteEdge` mutation lacks an attached session ID,
`MemoryStore` scans every session's snapshot to find the owning session.

**Note:** Fine for test workloads (single session). SQL adapters will use
indexed queries and don't have this issue.

#### I3-opus46: Graph lock discipline is clean — verified no `.await` under lock

**Location:** [`flush.rs` lines 272-278, 384](file:///home/nryn/work/lambo/src/store/flush.rs)

Verified three lock acquisition sites in `FlushTask`:
1. Line 272: READ lock for `session_id()` clone — dropped immediately.
2. Lines 273-278: WRITE lock for `drain_log()` — scoped block, dropped before
   any store I/O.
3. Line 384: READ lock for `log_len()` in `refresh_depth()` — sync function,
   no `.await`.

All three comply with spec §6.4. No findings.

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

## Downstream Guidance for T3.2 / T3.3

| Contract | Source | What adapters must do |
|---|---|---|
| Idempotent flush | M1-opus46 | Every mutation kind must use `ON CONFLICT` upsert semantics |
| Chronological order | `drain_log` contract | Replay mutations in submission order, never reorder into §2.4 phases |
| `reinforcements = 1` on creation | P2 I2 | Match graph core convention, not DDL default of 0 |
| Partial UNIQUE | T3.1 DDL | `ON CONFLICT` must spell `WHERE concept_type <> 'Observation'` |
| Timestamp format | T3.1 handoff | `chrono::to_rfc3339_opts(SecondsFormat::Millis, true)` — 24-char `YYYY-MM-DDTHH:MM:SS.SSSZ` |
| `vector_dimensions()` | Trait default | CockroachStore: `Some(1024)` from schema; SqliteStore: `None` |

## Verdict

**CONDITIONAL ACCEPT.** The store tier infrastructure (trait, registry,
MemoryStore, flush task, session loader) is well-engineered with 39 tests,
clean lock discipline, and proper panic containment. M1-opus46 (idempotency
contract documentation) must be addressed before T3.2/T3.3 start writing
their `flush()` implementations — it's a documentation fix, not a code change.
S2-opus46 (single-interaction coverage) should be fixed before P6 canonization
consumes `interaction_span`.
