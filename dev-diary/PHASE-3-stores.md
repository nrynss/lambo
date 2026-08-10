# P3 — Stores (durable tier)

```yaml
id:       P3
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
```
Full `GraphStore` impl over `sqlx::PgPool`. `flush()` applies a `MutationBatch` in one
transaction in spec §2.4 order: node upserts → edge upserts → deletions → canonization
transitions. `keyword_candidates` via SQL `LIKE`/full-scan is acceptable (RAM index is the
real path; this exists for reader processes). `vector_candidates` using the T0.3 spike's
encode/decode. Capabilities: `VECTOR_SEARCH`. `record_canonization` appends to
`canonization_events` — **this table is the demo's on-screen artifact; get it right.**

**Done when:** the T1.2 conformance suite passes against a live cluster (feature-gated
integration test, `LAMBO_COCKROACH_DSN` env), including `fixtures/mutations-batch.json`
round-trip.

---

### T3.3 — `SqliteStore`
```yaml
requires:   T1.2, T3.1
fixture-ok: yes  # sqlite::memory: — no external dependency at all
owns:       src/store/sqlite.rs
status:     not-started
```
Same shape over `sqlx::SqlitePool`. No `VECTOR_SEARCH` capability — `vector_candidates`
returns `StoreError::Unsupported`. Placeholder syntax (`?` vs `$1`) and interval arithmetic
(SQLite has no `INTERVAL`; compute cutoff timestamps in Rust and bind them — do the same in
T3.6 for both dialects to keep the queries twin-shaped). **Cut-order note:** 3rd in the cut
order, but it is also the test suite's fast path — cutting it late is expensive; finish it
early instead.

**Done when:** the conformance suite passes on `sqlite::memory:` in plain `cargo test`.

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
The spec §4.1 SQL verbatim for Cockroach; ported placeholders/intervals for SQLite. These
back canonization Stages 2–3 (spec §10) and the `⚑ Load-bearing pillar` warning — **on the
never-cut list.**

**Done when:** on `fixtures/session-rest-api.json` flushed into each store, both queries
return values equal to `MemoryStore`'s naive answers (three-way agreement test). That test
is the abstraction's proof.

---

## Exit criteria

- [ ] Conformance suite green ×3 (memory, sqlite, cockroach)
- [ ] Flush semantics: ordering, retry, degradation, observability all tested
- [ ] Round-trip load fidelity
- [ ] Three-way structural-query agreement

---

## Handoff Log

> _Fill on completion. Record the VECTOR encode/decode choice and every dialect divergence._
