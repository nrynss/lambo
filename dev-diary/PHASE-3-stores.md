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
status:     not-started
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
status:     not-started
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
status:     not-started
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
