# Issue #30: writing `access_count` and `last_accessed`

Status: implemented on `fix/30-access-count`, remediated after the Opus
review (narrow update, outage bound, close race, deterministic close tests,
live pg test). Decisions below are binding for #29 and #14 unless changed
explicitly.

## The defect

`Concept::access_count` / `last_accessed` had columns on all three dialects and
round-tripped through every adapter, but no production path wrote them. Spec §9
reserves 20% of the daemon composite for `frequency`; ALGO-1 renormalized it
away while it was dead, so GC's step 2 ranked eviction on recency, density and
type alone. On the Metal rig snapshot all 3,370 concepts read 0 after weeks of
daily recalls.

## Decisions

### 1. What counts as an access

- **Every concept hit a writer's recall returns, once per recall.** Counted
  from `Memory::recall_detailed`'s result, after the pipeline, so it covers the
  blended path, the structural-dispatch path and the cache-served path alike.
  - *Below the token cut counts too.* The MCP `structuredContent.hits` and the
    HTTP payload carry every hit's content, so the caller received it. Counting
    only `included_in_context` would make the same query count differently for
    callers with different `max_tokens`.
  - *A cached recall is still a recall* (#14). The cache stores the
    epoch-stable pipeline; assembly reruns per call, and the count is taken
    after it, so #14 moving the cache check ahead of the embed does not change
    what counts — the note must stay after the pipeline returns.
  - Interaction hits carry no counter and are skipped at apply time.
- **A resolved `lambo_inspect` counts its focus concept only.** It is a
  deliberate lookup of a named concept (AGENTS.md tells agents to inspect a
  resource before changing it: exactly what GC should keep). The neighbourhood
  is context around the answer, and depth 5 can reach hundreds of concepts.
  Ambiguous / missing / oversized refusals returned no concept and count
  nothing.
- **Not counted:** `lambo_saints` (a listing), `lambo_stats`, writes (they
  already move `created_at`/edges), and **reader processes** (`lambo recall`
  CLI, `serve-web`): they hold no lease and cannot write; they build a `Daemon`
  without a ledger. The CLI is operator inspection, not agent recall.
- **Proxies count once.** The J2 proxy is a byte pipe with no graph; the holder
  executes the forwarded `tools/call` through its own `Memory` and notes the
  hits there. Pinned by `tests/serve_proxy_multi_client.rs::
  a_recall_through_the_proxy_counts_once_in_the_holder`.

### 2. Write path and coalescing

```
recall / inspect ──note──▶ AccessLedger (leaf Mutex<HashMap<NodeId,(count,last)>>)
                                   │ take (ledger lock released)
daemon cycle, step 0 ──────────────▶ graph.write().record_accesses(batch)
                                   │   in RAM: access_count += n, last_accessed = max
                                   │   mark concept access-dirty (a set, ≤ 1 per concept)
                                   │   no log entry, epoch NOT bumped
flush cycle ───────────────────────▶ graph.drain_accesses(budget) → RecordAccess per concept
                                   │   only while pending holds no undelivered accesses;
                                   │   budget = log_max / 2 − pending
store ─────────────────────────────▶ UPDATE concepts SET access_count = max(stored, new),
                                         last_accessed = max(stored, new)  (existing rows)
Memory::close: ledger.close() (take + shut), then in the final drain's write section
               apply + drain_log + drain_accesses(all)
```

- The read path never takes the graph write lock and never mutates the graph
  under the read lock. The ledger is a leaf: never held while another lock is
  acquired (`record_never_waits_on_the_graph_lock`). Graph → hot-list order and
  no-lock-across-await are untouched; `run_cycle` stays synchronous.
- **Narrow, monotonic update (remediation, orchestrator decision).** The first
  cut emitted a full-row `UpsertNode` per touched concept, embedding included;
  on PostgreSQL that rewrites an indexed column (no HOT update) and on both pg
  dialects re-touches the vector index for a read. `Mutation::RecordAccess
  { session_id, id, access_count, last_accessed }` carries the concept's
  **absolute** values; every adapter applies it as a two-column `UPDATE` with
  `max(stored, new)` on each, existing rows of the same session only (SQLite:
  `UPDATE … FROM` a renamed `VALUES` table, `MAX` + `COALESCE`; PostgreSQL and
  CockroachDB: one shared, cast-free `UPDATE … FROM (VALUES …) AS v(…)` with
  `GREATEST` and INT8 binds — byte-identical, so it lives in the shared pg
  base, not `DialectSql`; MemoryStore mirrors it). One statement per chunk
  (`BulkLimits::accesses`: 240 rows on SQLite, 1,024 on pg).
- **Ordering against a concept upsert (defined and tested).** The planner
  buckets accesses into `FlushStep::Accesses`, emitted after the segment's
  concept upserts and flushed before any barrier; repeats collapse to each
  column's maximum. Order against an upsert of the same concept does not
  matter: the graph only raises the two fields, so whichever of the two was
  appended later carries the higher values, and the `max` makes the access
  update unable to lower what an upsert wrote. Pinned by
  `store::batch::tests::accesses_follow_the_segments_concepts_and_collapse_to_the_max`,
  `store::sqlite::tests::record_access_and_a_concept_upsert_in_one_batch_keep_the_later_values`
  and the live pg test; moving the bucket before the concepts, or dropping the
  `max`, fails them.
- **Outage bound (remediation).** The first cut appended one log entry per
  touched concept per tick; a store outage retains everything drained, so
  steady reads alone walked the backlog toward `backend_log_max` (50,000) and
  a terminal `durability="none"`. Accesses are now per-concept state (the
  dirty set) rather than log entries, and the flush takes them at most once
  per `pending` lifetime and at most `log_max / 2 − pending` at a time; a
  degraded task takes none. Chosen over "keep notes in the ledger while the
  flush is unhealthy" because GC and scoring keep seeing counts at most one
  tick old during an outage, and the flush loop — not the daemon — is the
  component that knows whether a batch is retained. Evidence:
  `store::flush::tests::read_traffic_during_an_outage_neither_grows_the_backlog_nor_degrades`
  (20 concepts read every second through 200 s of outage, `log_max` 100: depth
  stays at 20, never degrades, all 200 counts land after recovery; without the
  gate it fails at round 1 with depth 40) and
  `an_access_drain_is_capped_at_half_the_log_bound`.
- Write volume: one access row per touched concept per flush, not per hit or
  per recall (`many_reads_coalesce_into_one_update_per_concept`).
- Applied in RAM before GC in the same cycle, so a sweep scores against counts
  at most one tick old.
- Loss bound: a crash loses what had not reached the store (≤ one tick + about
  two flush intervals while healthy; for as long as an outage lasts
  otherwise, like any retained batch). A clean close loses nothing noted
  before close took the ledger. A batch the store rejects deterministically
  (dead-lettered, STORE-4/D5) takes its `RecordAccess` entries with it; the
  in-RAM values stay correct and the counts self-heal on the next read or
  upsert of the concept.
- **Close race (remediation).** A recall in flight when `close` took the
  ledger used to note into a ledger nothing would apply. `AccessLedger::close`
  now takes and shuts under one lock; a later note is dropped and counted
  (`dropped_after_close`; the first drop logs once at debug, the count is read
  through `AccessLedger::dropped_after_close`), never left pending
  (`notes_racing_close_land_in_the_batch_or_are_counted_dropped`,
  `a_read_finishing_after_close_is_dropped_not_left_pending`).
- Not a persisted or wire format: `Mutation` is applied in-process; the only
  serialized form is the fixtures' JSON loader, which gains an additive
  `record_access` tag. Public Rust API: `Mutation`, `FlushStep` and
  `BulkLimits` grew (CHANGELOG, Breaking).
- Access instants come from the wall clock, not `Memory`'s interaction clock:
  `lambo demo`'s script clock advances per call, so reading it would shift the
  script's later interactions by every retried recall. Scoring and the
  staleness detector therefore take `max(created_at, last_accessed)` as the
  last touch (a no-op for every production stamp).

### 3. Mutation accounting

Accesses **do not advance the mutation epoch.** `Graph::record_accesses`
changes RAM and the dirty set only, and `drain_accesses` hands the flush
mutations the epoch never counted, on the precedent of `record_write_intent`.
Consequences, each deliberate:

- **GC trigger:** `gc_interval` counts writes only; a read-heavy session does
  not sweep on reads (`applied_accesses_do_not_advance_the_gc_trigger`: 100
  accesses do not fund a sweep needing one mutation; the next write does).
  #29's idle floor (100 mutations) likewise counts writes only.
- **Recall cache:** not invalidated. Nothing the cached pipeline (phase 1 +
  expansion) reads changes. The daemon score table, which assembly reads, is
  rescored only on epoch change, so new frequency reaches recall *ranking* at
  the next real write — accepted: ranking between two writes stays stable and
  cacheable. GC does not use the table; it rescores from the graph.
- **Hybrid replanning:** an access between plan and commit does not force a
  replan; the commit stages on a clone of the live graph under the write lock,
  so the access is carried, not lost.
- **Durable watermark:** accesses never move the epoch; adapters persist `MAX`
  of whatever stamp a flush carries. Verified on restart
  (`accesses_survive_close_and_reattach_*`,
  `accesses_are_durable_across_a_writer_restart_on_sqlite`, the live pg test).
- **#29 interface:** a #29-side constraint recorded in the dogfood graph
  assumed access updates bump the epoch and would need
  `Graph::exempt_from_gc_measure`. They do not bump it, so they are outside
  GC's measure by construction and #30 calls no exemption.

The first cut had no separate dirty set (the log already carries mutations
the epoch does not count). The outage bound above is why it has one now.

### 4. Scoring with the dimension live

`score::score` is unchanged and thresholds are unchanged.

- **GC's cut has no cliff (landed with #29, which this branch is rebased
  onto).** The first cut of this branch found that ALGO-1's session-wide
  `frequency_is_live` switch would remove the 1.25x renormalization from every
  unread concept at the first applied access (the dry run below measured it
  against the pre-#29 cut). #29's additive rule replaced the switch in GC's
  cut: each concept scores the live-dimension score plus its own frequency
  term (`score::score_live_plus_frequency`), so a read can only raise that
  concept's eviction score. Pinned on the combined tree by #29's
  `an_access_on_another_concept_never_lowers_an_unread_concepts_gc_score`.
- **Nothing else has the cliff (verified).** Recall ranking, the daemon score
  table and canonization Stage 1 (its p90 is of peer scores from the same
  table) use `score()`, whose `SessionContext` does not depend on accesses: an
  access moves only the accessed concept's score
  (`score::tests::an_access_moves_only_the_accessed_concepts_score`, bit-exact
  for the unread concept).
- `MIN_CONCEPT_SCORE` was not moved by this branch; #29's cut and its
  protections define what it measures.
- **GC's recency anchor reads `last_accessed`.** #29's 365-day cut measures
  time since the later of `created_at` and `last_accessed`, so a recalled old
  Entity or isolated Resource is not collected on age
  (`a_recalled_old_entity_and_isolated_resource_survive_the_365_day_cut`).

### 5. Operator decisions (2026-10-07)

- **A read-only session no longer goes Stale.** Reads are activity: the
  staleness detector already read `last_accessed`, it now has something to
  read. Intended.
- **The daemon score table picks up frequency only at the next real write.**
  It rescores on epoch change and accesses do not move the epoch; ranking
  between two writes stays stable and cacheable. Accepted lag.
- **Narrow access update: implement** (done, §2).
- **Cockroach live CI stays off** (2026-10-06): the Cockroach live test exists
  behind `LAMBO_COCKROACH_DSN` but does not run in CI.

## Dry run (Metal rig snapshot copy `work30.db`, default weights; aggregates only)

Measured against GC's step-2 cut as it stood **before #29** (session-wide
ALGO-1 switch). History, not a calibration target: #29's additive rule
changes what the cut measures.

Method: a throwaway harness derived from #29's (`gc_dryrun30.rs`, never committed). The snapshot has
no call ledger, so access data is **synthetic**: for each interaction after the
first, *k* recalls through the real daemon pipeline (keyword + recent legs, no
vector leg, `top_k` 5, depth 2), each querying the first 12 words of a concept
the previous interaction derived, stamped at the interaction's `created_at`, run
against the final graph (a simplification: the recent leg returns the same
newest concepts on every replay, so one concept collects one access per recall).

| scenario | first sweep | Logic | Observation | Resource | Entity | 6 idle sweeps |
|---|---|---|---|---|---|---|
| S0 baseline (no access) | 537 | 42 | 324 | 171 | 0 | 574 |
| S1 one access, ALGO-1 switch only | 677 | 45 | 432 | 194 | 6 | — |
| S2 k=1 (1,146 recalls; 2,115 concepts read) | 492 | 19 | 320 | 152 | 1 | 524 |
| S2 k=2 (2,292 recalls; 2,671 read; p50 2, p90 8) | 398 | 7 | 266 | 124 | 1 | 424 |
| S2 k=4 (4,584 recalls; 3,065 read; p50 5, p90 16) | 249 | 2 | 158 | 88 | 1 | 257 |

k=2 vs baseline: 162 saved (36 Logic, 74 Observation, 52 Resource), 23 newly
collected. Frequency alone (same counts, `last_accessed` cleared) collects 426,
so roughly four fifths of the protection is `frequency`, one fifth recency.
Constraint is never collected in any scenario.

## Recall latency (fixture store, release build, 1,000 concepts, `top_k` 10)

Interleaved before (`c83b933`) / after runs, 10,000 recalls each, MemoryStore
(cache-served path): p50 per run before 510-606 µs (median 536), after
520-531 µs (median 528); best clean runs 513 vs 522 µs. SQLite (vector leg, no
cache, 1,000 recalls): p50 58.6-67.7 ms both sides. The difference is inside
run-to-run noise on a loaded machine; the added work is one leaf-mutex section
of ≤ `top_k` hash inserts per recall.

After the remediation (narrow update, dirty set, ledger close gate), same
harness, release build, interleaved with the two earlier binaries on the same
machine: MemoryStore p50 per run (10,000 recalls, 6 runs) base `c83b933`
530.8-620.4 µs (median 546), first cut 538.2-749.2 µs (median 574),
remediated 533.9-554.7 µs (median 540); SQLite p50 (1,000 recalls, 2 runs)
60.7-62.0 / 62.1-62.7 / 61.7-64.0 ms. Inside run-to-run noise; the read path
did not change (one leaf-mutex section per recall, plus a flag check).

## Interface with #29 (landed; this branch is rebased onto it)

- `last_accessed` is a real "last touch" for #29's 365-day recency anchor:
  reads count as touches (see §4).
- #29's mutation floor, `gc_idle_floor` and persisted `last_gc_epoch` read
  `Graph::epoch()`; accesses never move it, so they cannot satisfy either
  trigger (`applied_accesses_do_not_advance_the_gc_trigger`,
  `applied_accesses_do_not_satisfy_the_idle_floor_of_the_timed_trigger`).
- The daemon cycle's step 0 (apply ledger) runs before rescore/detect/GC;
  #29's timed trigger lives in step 3, so a sweep sees fresh counts.
- #29's additive step-2 rule is what GC's cut uses; the dry-run numbers above
  describe the pre-#29 cut and are history, not a calibration target.
- Accesses do not bump the epoch, so #29's `exempt_from_gc_measure` seam is not
  needed for them (§3).
- `lambo_stats` keeps #29's `gc` block; #30 adds no stats key
  (`stats_gc_block_is_unmoved_by_recall_and_inspect_accesses`).
