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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p3)
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

- [x] Conformance suite green ×3 (memory default; sqlite/cockroach under their features)
- [x] `build_store` registers both SQL adapters; uncompiled kinds still fail closed
- [x] Flush semantics: ordering, retry, degradation, observability all tested
- [x] Round-trip load fidelity
- [x] Three-way structural-query agreement (incl. §4.1 blast errata)
- [x] `sqlx` remains optional (not in default features)

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


### T3.2 — `CockroachStore` (done, task/p3-t3.2-cockroach-store)

**What exists:** `src/store/cockroach.rs` — full `GraphStore` over `sqlx::PgPool`
(feature `store-cockroach`), registered in `build_store`; `vector_dimensions() -> Some(n)`
parsed from the embedded DDL (`VECTOR(n)`, currently 1024 — **not** a global constant);
capabilities `VECTOR_SEARCH`. `flush` (one transaction, in-order replay), `load_session`
(all 7 tables incl. synonyms/reservations/canonization_events), `keyword_candidates`
(exact-substring `strpos` full scan; RAM index is the real path), `vector_candidates`
(T0.3 spike shape), `blast_radius` + `interaction_span` (spec §4.1 + errata), and
`record_canonization` (appends to `canonization_events` + updates the concept row).
Also a fixtures-gated `seed()` (full-snapshot path carrying synonyms/reservations —
MemoryStore parity) and an `init_schema()` that executes `migrations/cockroach/001_init.sql`
verbatim via `include_str!` + `sqlx::raw_sql` (multi-statement simple protocol;
idempotent — all `IF NOT EXISTS`).

**mod.rs touches (announced, shared file):** one additive `pub mod cockroach;`
(feature-gated), the cfg-gated `build_store` Cockroach arm (constructs lazily — pool
creation is deferred, so `build_store` stays sync and I/O-free), `is_ready()` Cockroach
arm flipped to `cfg!(feature = "store-cockroach")`, and the old
`cockroach_fail_closed_message` test rewritten as `cockroach_build_behavior` (branches on
`is_compiled()`: feature on → working adapter with VECTOR_SEARCH + Some(1024); off →
fail-closed rebuild hint). No other mod.rs lines touched — T3.3's Sqlite arms are
untouched and don't collide.

**Vector encode/decode decision (T0.3 spike, Attempt A):** bind the embedding as a text
literal and cast server-side (`$15::VECTOR`); read back via `embedding::STRING` + parse.
Text form `[x,y,z]` with Rust's shortest-round-trip `f32` Display (exact round-trip;
spike-verified). Non-finite elements rejected at encode. **Score = cosine similarity from
L2 distance** (`1 - d²/2`, clamped) so it is comparable to `semantic_match_threshold`
(spec §7.1 step 6); valid only for unit-normalized embeddings (pipeline normalizes).

**Batch replay decision (reviewer fodder):** `flush` replays mutations **in submission
order** — NOT re-grouped by spec §2.4 kind order. `src/graph/mod.rs` (T2.1 M2 close)
mandates "replay in order and MUST NOT re-sort"; §2.4's grouping holds *within* a logical
write, which the graph tier already guarantees when it emits the log. Re-grouping would
break create→delete→create within one batch. `MemoryStore` does the same.

**Dialect divergences (recorded):**
1. **Cockroach `INT` = INT8 on the wire.** All integer reads decode as `i64` then cast to
   the Lambo `i32` fields (`access_count`, `gc_survived`, `blast_radius`, `reinforcements`);
   `i32` binds are accepted by Cockroach (coerced). `LIMIT` bound as `i64`.
2. **Serializable-transaction retry is client-side.** Cockroach aborts conflicting
   serializable transactions with SQLSTATE 40001 (`restart transaction: ...
   RETRY_SERIALIZABLE`); sqlx does not auto-retry. `flush`/`seed`/`load_session`/
   `record_canonization` replay the whole transaction body (fresh BEGIN) up to 5 times
   with 50–200 ms backoff. Detection is string-marker based on Cockroach's stable
   "restart transaction"/"RETRY_SERIALIZABLE"/"40001" prefixes (the sqlx error is already
   flattened into `StoreError::Backend`); a typed error-code path is future work.
3. **§4.1 age filters bind a Rust-computed cutoff** (`now - min_age`), not an
   `INTERVAL` literal — T3.3/T3.6 twin-shape contract (SQLite has no `INTERVAL`).
   `interaction_span` additionally filters `edges.created_at` (spec's literal SQL only
   filters `i.created_at`) to match `MemoryStore`'s naive answer — the T3.6 three-way
   agreement baseline. Both queries also pin the source concept to the session
   (`src.session_id = $1`) and exclude the node itself (`c.id <> $2`), mirroring
   MemoryStore.
4. **rustls DSN rewrite** (T0.3 spike, proven): `sslrootcert=system` → real CA bundle or
   `sslmode=require`; dangling `?&`/trailing `&`/`?` cleaned. Required for the `.env` DSN.
5. **Sessions rows are ensured on flush** (`INSERT INTO sessions (session_id) ... ON
   CONFLICT DO NOTHING`) because Cockroach enforces `REFERENCES sessions(session_id)` on
   interactions/concepts (the graph tier creates sessions implicitly).
6. **Canonization event insert is `ON CONFLICT (id) DO NOTHING`** so a retried flush
   (committed but response lost) cannot duplicate the demo's audit row. Concept status
   update is idempotent by nature; node/edge upserts are `ON CONFLICT` DO UPDATE.

**Schema persistence (P3 review round 1 remediation, 2026-08-11):** (a) **`chunk_group_id`
(T2.5) is now a first-class `concepts.chunk_group_id STRING` column** — declared in the
`CREATE TABLE` for fresh installs plus `ALTER TABLE concepts ADD COLUMN IF NOT EXISTS
chunk_group_id STRING` for existing clusters (Cockroach supports `IF NOT EXISTS` on
`ADD COLUMN`), so `init_schema` stays idempotent (verified live ×2). The concept upsert
writes it (`$16`, included in the `ON CONFLICT` update) and `load_session` reads it back —
flush→load now **preserves** the T5.2 sibling co-retrieval key (round-trip asserts
survival, not the old normalization-to-None).
(b) **The `EmbeddingContract` is snapshot-only (S5-class).** `sessions` now carries
nullable `embedding_kind` / `embedding_model` / `embedding_dim` (same CREATE + ALTER
`ADD COLUMN IF NOT EXISTS` pattern), and `load_session` materializes
`GraphSnapshot.embedding` when `embedding_kind` is present. `flush` does **not** write
them — there is no session-metadata `Mutation` kind; the write path is pending a future
mutation (live suite stamps them via direct SQL and asserts flush immunity).
(c) `mutations-batch.json`'s final state is intentionally **not** a legal §5.7 graph (the
batch deletes the Temporal edge between the two interactions), so the flush round-trip is
asserted at snapshot level; `Graph::from_snapshot` materialization of legal batches is
covered by `load.rs` tests. (d) **Keyword scoring folds case** (`score_keyword_hits`
lowercases content/key before counting — MemoryStore parity); the SQL predicate already
lowercased, so a mixed-case row previously scored 0.0 despite being selected
(review R1, regression-locked vs MemoryStore live + pure unit test).
(e) **Legal-demote flush** (review R2): duplicate-key Observations flush successfully
against the live store — the partial `concepts_key_non_obs_idx` excludes them; a
duplicate-key non-Observation still fails loudly (negative lock in the same check).

**Conformance — ALL items RAN live** (cluster reachable via the `.env` DSN; never
printed): init_schema ×2 idempotent; `mutations-batch.json` flush + load round-trip
(2 interactions / 1 concept with status Candidate / 1 edge after delete + incident-edge
cleanup / 1 canonization event); a 1024-dim embedding write + `vector_candidates` top-1
(identical vector ranks first, score ≈ 1.0, stored vector round-trips exactly);
`keyword_candidates` on a planted concept (+ SessionNotFound and empty-token parity vs
MemoryStore); **mixed-case keyword scoring ranks exactly like MemoryStore** (review R1:
"Register User" + "user schema" vs tokens `[register, user]` — 2.0 then 1.0); **legal
demote flush** (review R2: duplicate-key Observations flush + survive; duplicate-key
Entity fails); **chunk_group_id round-trip** (Observation with `chunk-42` survives
flush→load; NULL stays None); **embedding-contract read** (direct-SQL stamp
kind/model/dim → `load_session` returns the contract; a later flush does not clobber it;
unstamped session → None); **`blast_radius` + `interaction_span` agreement vs MemoryStore
on `session-rest-api`** (all 22 concepts, min-age 0) plus a fresh-vs-aged-edge filter
agreement (min-age 0 vs 1h); `record_canonization` append + idempotent re-record +
NotFound; seed full-snapshot round-trip (synonyms, root_goal, deep-equal after
id-sort). Wired as `#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]`
`conformance_suite` — one `#[tokio::test]` running all checks on ONE runtime, because
sqlx connections are registered with the Tokio runtime that first acquires them: per-test
runtimes die at test end, poisoning the pool ("A Tokio 1.x context was found, but it is
being shutdown"), and per-test pools multiplied connections past the cluster's cap
("pool timed out while waiting for an open connection"). Skips cleanly (never fails)
without `LAMBO_COCKROACH_DSN`. `build_store_returns_working_adapter` is a plain sync
test (lazy pool ⇒ no runtime needed).

**Verification:** `cargo build --features store-cockroach` clean (0 warnings);
`cargo test --features store-cockroach` green (226 lib + main + integration, incl. live
suite); default-feature `cargo test --lib` green. 15 pure-logic unit tests (no
cluster): vector encode/decode round-trip + Cockroach renderings + non-finite
rejection, DDL width parse, age-cutoff computation, §4.1 placeholder counts/order +
errata shape, keyword-SQL placeholder arithmetic, keyword score case-folding
regression (R1), snapshot→row column counts (16-concept / 6-interaction / 9-edge),
enum↔STRING round-trips, serializable-retry detection, rustls DSN rewrite, dim check,
token normalization.


### T3.3 — SqliteStore (handoff, 2026-08-11, task/p3-t3.3-sqlite-store @ 9d95230)

**What exists:** `src/store/sqlite.rs` — full `GraphStore` over `sqlx::SqlitePool`
(one connection, `sqlite::memory:` for tests), registered in `build_store` for
`StoreKind::Sqlite` under feature `store-sqlite`. **mod.rs touches (shared-file
convention):** (a) `#[cfg(feature = "store-sqlite")] mod sqlite;` +
`pub use sqlite::SqliteStore;`, (b) the `StoreKind::Sqlite` `build_store` arm
replaces the "not implemented" Err, (c) `is_ready(Sqlite)` now returns
`cfg!(feature = "store-sqlite")`, (d) the obsolete `sqlite_fail_closed_message`
test rewritten as `sqlite_builds_or_fails_closed_by_feature` (it asserted
`!is_ready()`, which (c) invalidates). No other mod.rs changes; Cockroach arm
untouched.

**Conformance (12 tests in sqlite.rs, all feature-gated; 2 fixture-gated; suite
222 lib tests green ×6 runs, zero warnings):** init_schema ×2 idempotent;
mutations-batch.json flush+load deep-equals the MemoryStore oracle; T3.5
round-trip shape reused (graph ops → drain_log → flush → `load_session` →
deep-equal, chain order + index agreement); keyword_candidates (empty-token /
limit-0 / missing-session guards, oracle-equal); blast_radius + interaction_span
three-way agreement with MemoryStore on session-rest-api (all 34 nodes × 2 ages;
hard anchors: hub 1001 → 8, span distinct 6, coverage 25/55); canonization_events
append via mutation and record_canonization (NotFound on missing concept);
ISO-8601 fixed-format + lex-ordering + parse-back; demote duplicate Observation
keys pass while duplicate Entity key fails (partial-UNIQUE, transaction rolled
back); concurrent flushes across sessions; build_store(kind=sqlite) working
adapter + is_ready.

**Divergences from the Cockroach shape (twin notes for T3.2 / T3.6):**
1. **Lazy pool is REQUIRED, not cosmetic.** `build_store` runs in a **sync**
   startup context (`main.rs` → `resolve_backends`), and
   `SqlitePoolOptions::connect_lazy_with` spawns a maintenance task via
   `tokio::spawn`, which **panics outside a Tokio context**. SqliteStore holds
   `OnceLock<SqlitePool>` and creates the pool on first async use. T3.2's
   PgPool will hit the identical wall — plan for it.
2. **Cross-runtime connection-return quirk.** sqlx returns a pool connection via
   a spawned task; a blocked current-thread runtime never runs it, so
   `load_session`'s worker thread (load.rs) can time out acquiring against an
   in-flight return. The T3.5-shape test uses
   `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` for this
   reason; multi-thread runtimes (production) are unaffected.
3. **Fixed ms format truncates sub-ms instants** (by contract). `Utc::now()`
   does NOT round-trip exactly; ms-aligned timestamps do. Tests use whole-second
   clocks.
4. **Structural queries are MemoryStore-exact, not spec-text-exact:** blast
   radius adds `c.id <> $node` (spec text would count the node against itself);
   interaction span gates the **edge** age too (`e.created_at <= cutoff AND
   i.created_at <= cutoff`) because MemoryStore checks both. Both divergences
   are required for the T3.6 three-way gate; keep them in T3.2.
5. **keyword_candidates uses `instr(lower(...), ?) > 0`** (exact substring,
   LIKE-free) rather than `LIKE '%tok%'` — MemoryStore's `contains` has no
   wildcard semantics. T3.2 may use LIKE; only wildcard-bearing tokens would
   differ.
6. **canonization_events load order:** `ORDER BY occurred_at, id` — memory
   preserves insertion order; random-id ordering broke the append test.

**Schema gap — RESOLVED for SQLite (P3 wave 2 remediation, RemedT33Sqlite):**
`concepts.chunk_group_id` is now persisted. `migrations/sqlite/001_init.sql`
carries `chunk_group_id TEXT` inline in the CREATE TABLE (fresh DBs), and
`init_schema` converges pre-existing databases via a `PRAGMA table_info`-guarded
`ALTER TABLE concepts ADD COLUMN chunk_group_id TEXT` — **SQLite has no
`ADD COLUMN IF NOT EXISTS`** (verified: 3.53.4 rejects the syntax; the bundled
libsqlite3-sys is 3.46, also no). `flush` upserts it, `load_session` reads it
back, and the round-trip test now asserts the demote chunk id SURVIVES
(`Some("chunk-1")`) instead of normalizing it away. The spec errata record for
both dialects is Main's at integration; Cockroach remediates in its own
worktree (its dialect DOES support `ADD COLUMN IF NOT EXISTS`).

**Embedding contract columns added (S5-class, read-only):** `sessions` now
carries `embedding_kind` / `embedding_model` / `embedding_dim` (nullable,
guarded ALTER convergence like `chunk_group_id`). `load_session` reads them
into `GraphSnapshot.embedding` when present (partial rows error); `flush` does
NOT write them — no `Mutation` kind carries session metadata, write path awaits
a future session-metadata mutation. Today they are always NULL after a flush.

**ON CONFLICT + timestamp choices:** concepts target the `id` PK; the partial
index `(session_id, canonical_key) WHERE concept_type <> 'Observation'` is a
separate constraint — bare `ON CONFLICT (session_id, canonical_key)` errors at
runtime (T3.1, verified). Edges target `(source, target, edge_type)` (memory's
natural-key preference; graph core preserves id/created_at on reinforcement so
the DO UPDATE SET id = excluded.id is safe). Timestamps:
`to_rfc3339_opts(SecondsFormat::Millis, true)` everywhere; age cutoffs computed
in Rust and bound as TEXT so `<=` is lex-valid.

**Not persisted (by design, matches MemoryStore):** session-level
root_goal/created_at/closed_at AND the embedding contract columns (no Mutation
kind — None/absent on load, S5 read-only), synonyms and reservations (S5 —
full-snapshot path only, which has no SQLite impl yet), concept `embedding`
(BLOB unused — never read/written). chunk_group_id IS persisted now (see the
remediation note above).

### P3 partial-review fixes (2026-08-11, task/p3-partial-review-fixes @ base 8629879)

**What landed (F1–F6):**
- **F1 — single-interaction coverage = 1.0, all three stores (orig S2).**
  `interaction_span` returned coverage 0.0 when the session temporal extent is
  a single point (`sess_span <= 0`) even with `distinct >= 1`, which blocked
  canonization Stage 2 in short sessions. Now `distinct == 0` is the only 0.0
  case: memory.rs, the Cockroach `INTERACTION_SPAN_SQL` `CASE` `ELSE` arm, and
  the SQLite Rust-side coverage all return 1.0 for a non-empty span over a
  single-point extent (the Cockroach extent is never NULL while the span is
  non-empty — same session — so the `ELSE` arm is exactly that case). Each
  store gained a single-interaction coverage test (sqlite/cockroach also
  assert MemoryStore parity); the existing three-way agreement tests still
  pass unchanged.
- **F2 — sync load_session bridge timeout (orig S3).** The worker-thread
  bridge can block forever on a hung store. The store call now runs under
  `tokio::time::timeout` inside the private runtime, defaulting to
  `LOAD_SESSION_TIMEOUT` (30s) → `StoreError::Backend("load_session timed out
  after 30s")` on elapsed. Parameterized as the internal
  `load_session_with_timeout(store, session, timeout)`; the public sync fn
  calls it with 30s. Tested with a hanging-store mock (whose `load_session`
  never resolves) and a 50ms timeout.
- **F3 — retained batch backs off instead of retrying every tick (rev S1).**
  After exhausting `retries`, a retained batch waits `RETAINED_BACKOFF`
  (= the per-attempt backoff cap, 10s) before the next attempt re-enters the
  retry sequence — a permanently failing store no longer re-runs attempts +
  warn on every interval tick. Implemented as `FlushLoop::retry_after`
  (tokio-clock deadline) gating `cycle` before the flush; degrade (`log_max`)
  and the depth accounting are unchanged. Paused-time test: attempts stay flat
  across ticks until the hold elapses, then exactly one new attempt, then flat
  again; the three retained-batch tests now advance past the hold to observe
  recovery.
- **F4 — async load_session_async entry (rev M1).**
  `load_session_async(store, session)` is now the async CORE (store load →
  `Graph::from_snapshot` → `InvertedIndex::from_snapshot`; `SessionNotFound` →
  fresh empty session; corrupted snapshot → typed `StoreError::Invariant`);
  sync `load_session` is a thin wrapper running the core on the worker thread
  via the existing bridge (with F2's timeout). Sync semantics unchanged —
  existing load tests untouched and green; new async-context tests cover
  round-trip parity with the sync path and the missing-session contract.
- **F5 — flush idempotency contract on the trait (orig M1).** The
  `GraphStore::flush` docstring now requires upsert / `ON CONFLICT` semantics
  for every mutation kind: the flush task may replay a partially succeeded
  batch, so plain INSERTs are not acceptable (replay must converge, never
  duplicate or error). Both adapters already comply — comment-only change.
- **F6 — reservations reader filter doc (rev S2).** Both migrations'
  `reservations` table now carry a comment telling external SQL readers to
  filter `WHERE expires_at > now()` (Cockroach) /
  `WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')` (SQLite) — expired
  soft locks persist until overwritten. Comment-only change.

**Deferred / noted (no code):**
- **Graceful-shutdown final drain deferred to v0.7.0 (orig S1).** The flush
  task has no shutdown signal in v0.1 (task runs until the runtime drops it or
  the handle is aborted); a final drain on shutdown is a v0.7.0 item, not a
  P3 gap.
- **MemoryStore delete-scan (orig I1):** MemoryStore resolves deletes by
  scanning for the node id (no per-node session index) — O(N) per delete,
  fine at hackathon scale; a session→id map is the future fix.
- **Utc-now-in-queries (orig I2):** MemoryStore structural queries use the
  caller's clock (`Utc::now`) for age filters; tests needing determinism use
  `min_age` of zero or planted timestamps.
- **Timestamp precision (rev I1):** already satisfied — both adapters persist
  ms-precision RFC 3339 (`to_rfc3339_opts(SecondsFormat::Millis, true)` /
  Cockroach `TIMESTAMPTZ`), and SQLite's documented `now()` default is the
  ms-precision `strftime('%Y-%m-%dT%H:%M:%fZ','now')` equivalent.

**Files:** `src/store/memory.rs`, `src/store/load.rs`, `src/store/flush.rs`,
`src/store/mod.rs` (trait docstring only), `src/store/cockroach.rs`,
`src/store/sqlite.rs`, `migrations/cockroach/001_init.sql` +
`migrations/sqlite/001_init.sql` (comments only), this file. No Cargo.toml, no
frozen-file changes.

### T3.6 — Structural queries, both dialects (done, task/p3-t3.6-structural-queries @ base add3e17)

**What T3.6 delivered (the three-way agreement gate — the fns themselves already existed
from T3.2/T3.3, unchanged):**

**Consolidated + extended agreement matrix in BOTH adapter files** (per-adapter suites,
twin-shaped):
- **Both fixtures × every node × min-age {0, 3600s}.** `session-rest-api` (34 nodes:
  22 concepts incl. Canonical hub 1001, Venerable 1012, D1–D8 orphans C1013–C1020,
  P1/P2 peers, + 12 interactions) and `session-drift` (11 nodes: 9 concepts + 2
  interactions), flushed/seeded into the store AND a fresh MemoryStore; every
  `blast_radius` and `interaction_span` (distinct AND coverage) asserted EXACTLY equal
  to MemoryStore's naive answer on the same snapshot. **Matrix dimensions: 45 nodes ×
  2 ages × 2 queries = 180 equality assertions per adapter.**
- **§4.1 errata probes (un-orphaning).** Mirror of MemoryStore's
  `blast_radius_ignores_provenance_derives_edges`: a concept whose only structural
  inbound is a Dependency from the probe node PLUS the mandatory §5.7 `Derives`
  provenance edge (interaction → concept) must still count as an orphan
  (`blast_radius == 1` at both ages) — if `Derives` were counted as "another source"
  every legal graph would show blast radius ~0. A negative probe adds a concept whose
  ONLY inbound is `Derives` (never an orphan of anyone). Added to BOTH adapters
  (sqlite test + cockroach live check).
- **Aged-vs-fresh edge interaction.** An aged `pillar → orphan` Dependency plus a
  freshly-created `other → orphan` Dependency: at min-age 0 the fresh edge un-orphans
  (radius 0), at 3600s it is filtered (radius 1); BOTH cutoffs asserted equal to
  MemoryStore on EVERY node of the synthetic session (sqlite test + extended cockroach
  live check — the latter previously probed only the pillar node).
- **F1 single-point coverage case retained** in both suites (coverage 1.0 when the
  session extent is a single point and distinct >= 1; 0.0 iff distinct == 0).

**Divergences found: NONE.** Both SQL dialects matched MemoryStore exactly on every
matrix cell on first run (the queries were already MemoryStore-exact from T3.2/T3.3 +
F1; the matrix is the proof, extended now to drift + age-3600 + errata + aged-edge).

**Documentation:** each adapter's module doc now carries a "Structural queries"
section spelling the semantics: errata exclusions (`Dependency`/`Causal`/`Hierarchical`
only; provenance `Derives`/`Temporal` never un-orphan), session-scoped source-concept
join (an interaction id can never be a structural source), `c.id <> $node`
self-exclusion, aged-edge gating (`e.created_at <= cutoff` in BOTH queries — spec §4.1
second errata; span also gates `i.created_at <= cutoff`), the F1 single-point rule, and
the twin-shaped Rust-computed cutoff (no `INTERVAL`).

**Round-1 review remediation (ReviewT36Structural, 2026-08-11):** two findings
fixed on `task/p3-t3.6-structural-queries`:
- **F1 [P2] — the span's both-timestamp gates are now discriminated
  behaviorally.** Previously every span test collapsed to an all-old origin
  set, so dropping either `e.created_at`/`i.created_at` gate passed the whole
  suite. The aged-vs-fresh test (both adapters) now gives the fresh edge's
  source a **DISTINCT origin interaction** (`i2`), so `span(orphan).distinct`
  is 2 at min-age 0 and 1 at 1h (fresh edge's origin drops out of the span
  when the edge is filtered), and adds an **i-gate probe** — an AGED edge
  (`probe_src → probe_victim`) whose origin interaction `i3` is FRESH:
  `span(probe_victim).distinct` is 1 at min-age 0 and 0 at 1h. Divergent
  values are asserted equal to MemoryStore on the same scenario (the matrix
  loop now covers 8 nodes × 2 ages × 2 queries), plus oracle-independent
  anchors; a `blast_radius` anchor proves the i-gate is span-only (origin age
  never affects radius). SQLite also gained a text-level lock mirroring
  Cockroach's `structural_query_placeholder_order_and_counts`
  (`structural_span_sql_gates_both_timestamps`: the span SQL contains BOTH
  `e.created_at <= ?` and `i.created_at <= ?`). Demonstrated: dropping the
  e-gate alone makes `span(orphan)` at 1h return distinct 2 vs MemoryStore's
  1; dropping the i-gate alone makes `span(probe_victim)` at 1h return
  distinct 1 vs 0 — both fail the matrix agreement. (`INTERACTION_SPAN_SQL`
  was extracted to a module const in sqlite.rs; behavior unchanged.)
- **F2 [P3] — the cockroach matrix count is now locked.** The twin
  `check_fixture_structural_agreement` returned its assertion count and
  `check_structural_queries_agree_with_memory_store` asserts the total == 180
  (45 nodes × 2 ages × 2 queries), matching the sqlite matrix lock — a future
  narrowing can no longer silently shrink coverage.

**Verification:** `cargo test --features store-sqlite` → 232 lib tests green (incl. the
extended matrix + the F1/F2 remediation probes; acceptance was 229+).
`cargo test --features store-cockroach` with the live `.env` DSN (never printed) → 232
lib tests green, `conformance_suite` RAN live (zero SKIP), incl. both-fixture matrix
(with the 180-assertion lock) + errata probe + aged-edge (F1-remediated, both-gate
discrimination) + single-point checks. Zero new warnings in both feature builds. No
changes outside `src/store/cockroach.rs` + `src/store/sqlite.rs` + this file.
