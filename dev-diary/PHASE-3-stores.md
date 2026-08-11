# P3 — Stores (durable tier)

```yaml
id:       P3
branch:   phase/p3-stores
requires: [P1, T0.3]
blocks:   P8; P6 swaps to live queries when T3.6 lands
parallel: high   # T3.1 first; then T3.2 ‖ T3.3; T3.4 ‖ T3.5 against MemoryStore meanwhile
runs-parallel-with: P2, P4, P5, P6, P7
```

**Goal:** `CockroachStore`, `SqliteStore`, the write-behind flush task, and session load —
spec §2.3–§2.5, §3, §4. The tier boundary is real: fast recall in RAM, structural truth in
SQL.

**Design rule (spec §3.1):** the interface speaks Lambo's vocabulary, never the database's.
Each adapter owns its SQL; runtime `sqlx::query`, no macros.

**Level B packaging** (see [`notes/level-b-pluggability.md`](notes/level-b-pluggability.md)):

| Adapter | Cargo feature | `store.kind` | Status |
|---------|---------------|--------------|--------|
| `MemoryStore` | `store-memory` (default) | `memory` | done (T1.2) |
| `CockroachStore` | `store-cockroach` | `cockroach` | T3.2 |
| `SqliteStore` | `store-sqlite` | `sqlite` | T3.3 |

Registry: `store::build_store(StoreConfig)`. Selecting an uncompiled kind fails closed.
`sqlx` is optional and pulled only by the SQL store features.

---

## Integration contracts from P2 review closes (2026-08-11)

Binding notes for P3 tasks; sources: muse-spark / grok branch reviews (CLOSED),
spec §4 errata. Do not re-derive — the graph tier already enforces all of these in RAM.

- **Partial UNIQUE index (M1/M2 → T3.1 DDL).** The schema's
  `UNIQUE (session_id, canonical_key)` is **partial**: non-Observation concepts
  only (`CREATE UNIQUE INDEX ... ON concepts (session_id, canonical_key)
  WHERE concept_type <> 'Observation'` — spec errata). Observations may share
  keys (demote's context-overflow duplicates). `Graph::insert_concept` and
  `assert_invariants` enforce the same rule in RAM; the DDL must mirror it or a
  legal demote will fail the store's upsert.
- **Reservations durability (S5 → T3.4/T3.5).** Reservations are RAM-local: no
  `Mutation` kind exists, the write-behind log never carries them. They persist
  ONLY via full `GraphSnapshot` save/load. `load_session` restores them from the
  snapshot; the flush task must not expect reservation mutations.
- **InvertedIndex owner wiring (M3 → whoever builds `Memory`).** The graph
  module is index-free by design; the session owner MUST mirror every concept
  write into `InvertedIndex` (`add` on create/update, `remove` on delete) —
  contract documented in `src/graph/mod.rs` and proven by
  `tests/p2_integration.rs::inverted_index_manual_sync_contract`. A forgotten
  mirror is silent recall staleness.
- **`reinforcements` convention (I2 → T3.2/T3.3 upserts).** Graph core counts
  creation as the first write (`reinforcements = 1`); the DDL default is 0.
  Store upserts must match the graph core's values, not the DDL default.
- **Scaling note (G4).** The in-RAM UNIQUE check is an O(N) scan per
  `insert_concept` (no canonical-key index in RAM — deliberate cut); the store
  side has the partial index. Do not benchmark P2 code against this as a hot
  path without an index.

---

### T3.1 — Schema DDL, both dialects
```yaml
requires:   T0.3
fixture-ok: n/a
owns:       migrations/cockroach/ (shared with T0.2 — coordinate), migrations/sqlite/
status:     claimed:main-swarm (worktree worktrees/p3-t3.1-schema-ddl)
```
Spec §4 DDL as the Cockroach source of truth (T0.2 applied it; this task makes it the
checked-in, adapter-consumed copy). SQLite translation: `STRING`→`TEXT`,
`TIMESTAMPTZ`→`TEXT` ISO-8601 (document the choice), `JSONB`→`TEXT`, `VECTOR(1024)`→`BLOB`
(unused — SQLite has no `VECTOR_SEARCH` capability), no vector index, `INDEX` clauses as
separate `CREATE INDEX`. `init_schema()` executes these idempotently.

**Done when:** both adapters' `init_schema()` runs twice cleanly on fresh targets.

---

### T3.2 — `CockroachStore`
```yaml
requires:   T1.2, T3.1
fixture-ok: no   # needs the T0.2 cluster; unit-level SQL shape tests only
owns:       src/store/cockroach.rs
status:     not-started
feature:    store-cockroach
```
Full `GraphStore` impl over `sqlx::PgPool`, gated on feature `store-cockroach`, registered
in `build_store` for `StoreKind::Cockroach`. `flush()` applies a `MutationBatch` in one
transaction in spec §2.4 order: node upserts → edge upserts → deletions → canonization
transitions. `keyword_candidates` via SQL `LIKE`/full-scan is acceptable (RAM index is the
real path; this exists for reader processes). `vector_candidates` using the T0.3 spike's
encode/decode. Capabilities: `VECTOR_SEARCH`. **`vector_dimensions() -> Some(n)`** where
`n` is the schema column width (Cockroach `VECTOR(n)` — typically 1024 for the demo DDL;
**not** a global constant in the embedder factory). `record_canonization` appends to
`canonization_events` — **this table is the demo's on-screen artifact; get it right.**

**Done when:** the T1.2 conformance suite passes against a live cluster (feature-gated
integration test: `cargo test --features store-cockroach` + `LAMBO_COCKROACH_DSN`),
including `fixtures/mutations-batch.json` round-trip; `build_store(kind=cockroach)` returns
a working adapter; `vector_dimensions()` matches the DDL so `resolve_backends` can reject
mismatched embedders.

---

### T3.3 — `SqliteStore`
```yaml
requires:   T1.2, T3.1
fixture-ok: yes  # sqlite::memory: — no external dependency at all
owns:       src/store/sqlite.rs
status:     claimed:main-swarm (worktree worktrees/p3-t3.4-flush-task)
feature:    store-sqlite
```
Same shape over `sqlx::SqlitePool`, gated on feature `store-sqlite`, registered in
`build_store` for `StoreKind::Sqlite`. No `VECTOR_SEARCH` capability — `vector_candidates`
returns `StoreError::Capability`. Placeholder syntax (`?` vs `$1`) and interval arithmetic
(SQLite has no `INTERVAL`; compute cutoff timestamps in Rust and bind them — do the same in
T3.6 for both dialects to keep the queries twin-shaped). **Cut-order note:** 3rd in the cut
order, but it is also the test suite's fast path — cutting it late is expensive; finish it
early instead.

**Done when:** the conformance suite passes on `sqlite::memory:` under
`cargo test --features store-sqlite` (not required in default features);
`build_store(kind=sqlite)` returns a working adapter.

---

### T3.4 — Write-behind flush task
```yaml
requires:   T1.1, T1.2
fixture-ok: yes  # runs against MemoryStore; adapters slot in later
owns:       src/store/flush.rs
status:     claimed:main-swarm (worktree worktrees/p3-t3.5-load-session)
```
Tokio task draining the T2.1 mutation log per spec §2.4–§2.5 semantics: flush on
`backend_flush_interval` (1s) or `backend_flush_max_batch` (500); on failure exponential
backoff × `backend_flush_retries=3`, then retain batch, raise `BackendFlushFailed` warning,
keep accumulating; past `backend_log_max=50_000` degrade to `durability="none"` at `ERROR`.
Expose `flush_lag` and `log_depth` for `stats()` (spec §2.4: the loss bound is *observable*,
not assumed). Take the graph lock only to drain — never across the store `.await`.

**Done when:** tests with a failing-then-recovering mock store show: session uninterrupted,
bounded retry, degradation past the cap, lag/depth accurate throughout.

---

### T3.5 — `load_session()` / startup
```yaml
requires:   T1.2
fixture-ok: yes
owns:       src/store/load.rs
status:     not-started
```
Spec §2.5: existing session → full graph materialized into RAM (nodes, edges, synonyms,
canonization statuses, temporal chain) and rebuilt indexes (calls T2.6's rebuild-from-graph
path); missing session → empty graph. Daemon warm-up rescore is the daemon's job — emit the
signal, don't implement it.

**Done when:** flush-to-store → drop → `load_session` → graph deep-equals the original
(round-trip test on MemoryStore and SQLite; Cockroach behind the integration gate).

---

### T3.6 — Structural queries, both dialects ★
```yaml
requires:   T3.2, T3.3
fixture-ok: no
owns:       (blast_radius + interaction_span fns inside cockroach.rs / sqlite.rs — same owner as T3.2/T3.3, claim jointly or sequence)
status:     not-started
```
The spec §4.1 SQL for Cockroach (**errata:** only concept-sourced
`Dependency`/`Causal`/`Hierarchical` edges — same as `MemoryStore`; provenance
`Derives`/`Temporal` must not un-orphan); ported placeholders/intervals for SQLite. These
back canonization Stages 2–3 (spec §10) and the `⚑ Load-bearing pillar` warning — **on the
never-cut list.**

**Done when:** on `fixtures/session-rest-api.json` flushed into each store, both queries
return values equal to `MemoryStore`'s naive answers (three-way agreement test). That test
is the abstraction's proof — and proves Level B adapters honor the same trait contract.

---

## Exit criteria

- [ ] Conformance suite green ×3 (memory default; sqlite/cockroach under their features)
- [ ] `build_store` registers both SQL adapters; uncompiled kinds still fail closed
- [ ] Flush semantics: ordering, retry, degradation, observability all tested
- [ ] Round-trip load fidelity
- [ ] Three-way structural-query agreement (incl. §4.1 blast errata)
- [ ] `sqlx` remains optional (not in default features)

---

## Handoff Log

> _Fill on completion. Record the VECTOR encode/decode choice and every dialect divergence.
> Confirm feature flags and `build_store` arms for cockroach/sqlite._

### T3.1 — Schema DDL, both dialects (2026-08-11)

**Committed:** `task/p3-t3.1-schema-ddl` — see commit for `migrations/cockroach/001_init.sql` +
`migrations/sqlite/001_init.sql`.

- **Partial-UNIQUE errata in BOTH dialects.** Table-level
  `UNIQUE (session_id, canonical_key)` removed from `concepts` (Cockroach and SQLite);
  both files carry the spec §4 errata comment + a separate idempotent statement:
  `CREATE UNIQUE INDEX IF NOT EXISTS concepts_key_non_obs_idx ON concepts (session_id, canonical_key) WHERE concept_type <> 'Observation';`
  Smoke-verified on SQLite: duplicate non-Observation key rejected, duplicate Observation
  key accepted.
- **No T0.2 peer exists** (file is T0.2's copy checked in at base 9f9d2cb); updated it
  in place — only the errata change, everything else as-is.
- **Dialect divergences (recorded for T3.2/T3.3):**
  1. Cockroach `STRING`/`UUID`/`JSONB`/`TIMESTAMPTZ`/`INT`/`FLOAT` → SQLite
     `TEXT`/`TEXT`/`TEXT`/`TEXT`/`INTEGER`/`REAL`.
  2. `TIMESTAMPTZ` → TEXT ISO-8601 UTC (`2026-08-11T12:41:19.186Z`), chosen so RFC 3339
     lexicographic order = time order → adapters can compare in SQL. Adapters MUST write/read
     a FIXED serialization: chrono `to_rfc3339_opts(SecondsFormat::Millis, true)` →
     `YYYY-MM-DDTHH:MM:SS.SSSZ` (24 chars, ms always present, `Z` not `+00:00`), keeping SQL
     lex comparisons valid. chrono's default `to_rfc3339()` is NOT acceptable — it emits
     `+00:00` and variable-width fractional seconds, breaking lex comparisons against this
     format.
  3. `now()` default → `strftime('%Y-%m-%dT%H:%M:%fZ','now')` (documented equivalent of
     CURRENT_TIMESTAMP; keeps the ISO-8601 format uniform, adds ms precision).
  4. `embedding VECTOR(1024)` → `BLOB`, **unused** (no VECTOR_SEARCH in SQLite; adapters
     never read/write it). No vector index in SQLite; Cockroach keeps
     `CREATE VECTOR INDEX IF NOT EXISTS concepts_embedding_idx`.
  5. Spec `INDEX (...)` clauses → separate `CREATE INDEX IF NOT EXISTS` statements in
     SQLite. Table-level `UNIQUE`/`PRIMARY KEY` constraints stay inline (SQLite autoindexes
     them — required for `ON CONFLICT` targeting).
  6. `REFERENCES` clauses kept in SQLite for fidelity; enforcement needs
     `PRAGMA foreign_keys = ON` at runtime — the adapter's job.
- **SQLite idempotency smoke:** `sqlite3 /tmp/lambo-t31.db < migrations/sqlite/001_init.sql`
  run twice → exit 0 both runs (3.53.4 CLI). All 7 tables + `concepts_key_non_obs_idx`
  (partial) + 5 regular indexes present. Cockroach DDL not runnable offline (no cluster);
  T3.2 conformance verifies it.
- **T3.3 notes:** FK enforcement + `ON CONFLICT` targets as above; `interactions.created_at`
  has NO default (matches spec — adapter must bind it). Upserts against the partial index
  MUST spell the conflict target with the exact WHERE clause:
  `ON CONFLICT (session_id, canonical_key) WHERE concept_type <> 'Observation'
  DO NOTHING` (or `DO UPDATE`). A bare `ON CONFLICT (session_id, canonical_key)` errors at
  runtime — "does not match any PRIMARY KEY or UNIQUE constraint" — because the table-level
  UNIQUE was removed (verified on SQLite 3.53.4).
- Status: lines not edited per task constraints (Main claims centrally).
### T3.5 — `load_session()` / startup (done, task/p3-t3.5-load-session @ 9f9d2cb+)

**What exists:** `src/store/load.rs` — `LoadedSession { graph, index }` + `load_session(&dyn GraphStore, &SessionId) -> Result<LoadedSession, StoreError>`, wired by one additive `pub mod load;` in `src/store/mod.rs`. Semantics per spec §2.5: existing session → `Graph::from_snapshot` (re-verifies §5.7 invariants; typed `StoreError::Invariant` on corruption, never a panic) + `InvertedIndex::from_snapshot` on the same snapshot; `SessionNotFound` → fresh empty `LoadedSession`; any other store error propagates. No `Utc::now` anywhere; all timestamps come from the snapshot.

**Rescore seam:** documented in the module doc, not implemented: the session owner (`Memory`, T2.3+) emits the signal only when an *existing* session was loaded. The signal is the warm-up rescore the T4.1 daemon task skeleton is *intended* to wake on (planned, not yet present), transported via the T4.6 event channel — today only `DaemonEvent` in `crate::types` exists; neither transport nor skeleton is in the tree.

**Sync-over-async bridge (surprise):** the pinned API is a sync `fn` but `GraphStore::load_session` is async and there is no `futures` dependency. `Handle::block_on`/`Runtime::block_on` panic when called from inside a tokio task (the likely startup context), so `load_session` runs the store future on a private worker thread with a fresh current-thread runtime. Startup-only cost, correct from both sync and async callers (tested both ways). Reviewer should scrutinize `block_on` in `load.rs`.

**Round-trip reality check (S5):** the write-behind log carries no synonym/reservation mutations, so a flush → load round-trip restores synonyms/reservations as **empty** — the S5 contract, asserted explicitly in the flush round-trip test. They survive only the full-`GraphSnapshot` path (`MemoryStore::seed` under `fixtures`), covered by `full_snapshot_round_trip_preserves_ram_local_metadata` (fixtures-gated). The acceptance's "reservation preserved" therefore lives in the full-snapshot test, not the flush test — the parent agent should confirm this split matches intent.

**Graph-tier behaviors noticed (do not re-derive):**
- `Graph::from_snapshot` requires exactly one temporal-chain head, so a present-but-*empty* snapshot (e.g. a session whose last node was deleted) loads as `StoreError::Invariant("expected exactly one chain head, found 0")`, not as an empty graph. Only `SessionNotFound` yields an empty session.
- `from_snapshot` replays edges via `record_edge`, so a corrupted snapshot with duplicate natural keys would silently *reinforce* (bump weight/reinforcements) rather than error. Legal snapshots are unique-keyed, so round-trip is exact.

**Tests (6, in `load.rs`, all generic vs `&dyn GraphStore` except the `MemoryStore::seed` one):** flush round-trip (interactions/derive/demote/synonym/reservation/canonization build → drain → flush → load; deep-equality incl. chain order, index agreement on fixture queries, invariants hold), full-snapshot metadata round-trip (fixtures), missing-session → empty, corrupted snapshot ×2 (dangling `previous_id`; concept without `Derives`) → typed error, sync-context (no runtime) load, and a mutation-log shape sanity test. Same round-trip shape should run against `SqliteStore` unchanged once T3.3 lands — only `MemoryStore::new()` needs swapping.

**Files:** `src/store/load.rs` (new), `src/store/mod.rs` (one `pub mod load;` line). No Cargo.toml, no frozen-file changes.
### T3.4 — Write-behind flush task (handoff, 2026-08-11)

**What exists:** `src/store/flush.rs` — `FlushParams`, `FlushStats`, `FlushTask`
(`new` / `spawn` / `stats` / `degraded`) + 6 paused-time tests; one additive
`pub mod flush;` in `src/store/mod.rs`. Works against any `GraphStore`
(MemoryStore in tests; Cockroach/SQLite slot in via T3.2/T3.3).

**Semantics implemented (spec §2.4–§2.5):**
- Interval loop drains `Graph::drain_log` under the WRITE lock only (never
  across `.await` — spec §6.4) and flushes the pending batch (retained first,
  then newly drained, chronological — mod.rs contract, never re-sorted).
- `max_batch` is a REAL early-flush trigger: the loop polls every
  `POLL_QUANTUM` (100ms) between ticks via `tokio::select!`, so a batch
  reaching `max_batch` flushes before the interval. The graph has no write
  channel, so the quantum is the trigger granularity.
- Failure: exponential backoff 100ms doubling (cap 10s) up to `retries` retries
  after the initial attempt (total attempts = retries + 1). Exhausted →
  batch RETAINED in `self.pending` (never dropped), `warn!("BackendFlushFailed")`,
  session keeps accepting writes (graph is the primary tier).
- Degradation: depth (pending + in-graph log) > `log_max` → `error!("FlushDegraded")`,
  `degraded() == true`, flushing stops permanently (durability="none").
- `stats()`: `lag` = time since last successful flush (tokio clock — virtual
  under paused time), `depth` = pending batch + in-graph log (the observable
  loss bound). Both refreshed each cycle (≤100ms staleness).

**API deviation to review:** `spawn(&self)` instead of the pinned `spawn(self)`
— `spawn(self)` would consume the caller's only path to `stats()`/`degraded()`
(they read Arc-shared state; the task clones the arcs). Same name, changed
receiver; `FlushTask` stays the caller's stats handle.

**Time-control gotchas (reported for the phase reviewer):**
- `tokio::time::advance` fires timers but only runs the woken task once the
  test yields — every test wait must use an initially-FALSE condition (e.g.
  `flush_calls >= n`, `log_len == 0`), never one true in the stale state
  (`depth == 0` right after spawn is stale until the first cycle).
- `lag` MUST use `tokio::time::Instant` (not `std::time::Instant`) or paused
  tests see real-time lag. Done.
- Retry backoff sleeps are `tokio::time::sleep` so the 100/200/400ms schedule
  is exactly controllable with `advance`.

**Fixture-ok track:** tests use MemoryStore + a `FlakyStore` mock (delegates to
an inner `Arc<dyn GraphStore>`, fails the first N flush calls / forever,
records call count + batch sizes). Adapters slot in with zero changes.

**Not wired yet (by design):** nothing consumes `FlushTask` — the daemon/session
owner (T4.x / P3 Memory) will spawn it. Reservations never enter the log
(S5 contract) — nothing to do here. `cargo test --lib` green (202 tests).

**Round-1 review remediation (2026-08-11):**
- **Spawn-once contract (F1).** `spawn(&self)` is kept (reviewer-accepted
  deviation), but `Shared` now carries a `started: AtomicBool`; `spawn`
  check-and-sets it and a second call panics ("FlushTask::spawn called twice —
  exactly one loop may run") — two concurrent loops with independent `pending`
  buffers could persist out of order. Callers must `spawn` exactly once.
- **Panic containment (F3).** `store.flush` is polled inside
  `catch_unwind` (std-only `CatchUnwindPoll` combinator); a panicking backend
  is treated as a failed attempt (`warn!("BackendFlushPanic")` → backoff →
  retain/degrade), so the loop can never die silently with `degraded()==false`.
- **Lag init at spawn (F4).** `last_success` is initialized in `spawn`, not
  `new` — `stats().lag` is 0 until the first successful flush after spawn even
  for a task held before spawning (matches the `FlushStats::lag` docstring).
- **Test-only flake fix.** The shared `BackendFlushFailed` warn callsite could
  register `Interest::never` (process-wide, first registration wins) in the two
  subscriber-less flush tests, silently disabling that event for the capturing
  assertions in `degrades_past_log_max_and_stops_flushing` under parallel
  execution. Those tests now install a silent TRACE-level default
  (`keep_callsites_enabled`) that registers callsites as `always`.
