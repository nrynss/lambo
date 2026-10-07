# Issue #30: writing `access_count` and `last_accessed`

Status: implemented on `fix/30-access-count`. Decisions below are binding for
#29 and #14 unless changed explicitly.

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
                                   │   access_count += n, last_accessed = max
                                   │   one UpsertNode per concept, epoch NOT bumped
flush task (unchanged) ────────────▶ store upsert of existing columns
Memory::close: take ledger, then in the final drain's write section apply + drain
```

- The read path never takes the graph write lock and never mutates the graph
  under the read lock. The ledger is a leaf: never held while another lock is
  acquired (`record_never_waits_on_the_graph_lock`). Graph → hot-list order and
  no-lock-across-await are untouched; `run_cycle` stays synchronous.
- Write volume is one `UpsertNode` per *touched concept per tick*, not per hit
  or per recall (`many_reads_coalesce_into_one_upsert_per_concept`).
- Applied before GC in the same cycle, so a sweep scores against counts at most
  one tick old.
- Loss bound: ≤ one daemon tick + one flush interval of counts on a crash; none
  on a clean close.
- `UpsertNode` rewrites the full row (embedding included), the same cost as
  `bump_gc_survived`. A narrow `UPDATE … SET access_count, last_accessed`
  mutation kind would be cheaper but touches every adapter and the batch
  planner; not worth it at current volume. Revisit if flush profiles show it.
- Access instants come from the wall clock, not `Memory`'s interaction clock:
  `lambo demo`'s script clock advances per call, so reading it would shift the
  script's later interactions by every retried recall. Scoring and the
  staleness detector therefore take `max(created_at, last_accessed)` as the
  last touch (a no-op for every production stamp).

### 3. Mutation accounting

Accesses **do not advance the mutation epoch.** `Graph::record_accesses`
appends to the log without `append_mutation`'s bump, on the precedent of
`record_write_intent`. Consequences, each deliberate:

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
- **Durable watermark:** the drained batch stamps the unchanged epoch; adapters
  persist `MAX`. Verified on restart (`accesses_survive_close_and_reattach_*`,
  `accesses_are_durable_across_a_writer_restart_on_sqlite`).

No separate dirty set was needed because the log already carries mutations
the epoch does not count (write intents).

### 4. Scoring with the dimension live

`score::score` and the ALGO-1 switch are unchanged; thresholds are unchanged.
Findings from the dry run below:

- **The switch is a cliff.** `frequency_is_live` is session-wide, so the first
  applied access removes the 1.25× renormalization from every concept at once.
  A single access on one concept raises a first sweep from 537 to 677.
- **A plausible read load more than pays it back** (398 at two recalls per
  interaction), but concepts read once or twice can still fall: 23 concepts
  survive the baseline and are collected with accesses live, 14 of them read.
- `MIN_CONCEPT_SCORE` was not moved: the evidence points both ways depending on
  read volume, and #29 is changing what step 2 measures. #29 must account for
  the cliff (operator constraint 2026-10-07).

## Dry run (Metal rig snapshot copy `work30.db`, default weights; aggregates only)

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

## Interface with #29

- `last_accessed` is now a real "last touch" for #29's "anchor recency to time
  since last touch": reads count as touches.
- #29's mutation-floor and persisted `last_gc_epoch` should keep reading
  `Graph::epoch()`: accesses never move it, so no adjustment is needed.
- The daemon cycle gained step 0 (apply ledger) before rescore/detect/GC; #29's
  timed trigger edits step 3. Merge order does not matter, but keep step 0
  first so a sweep sees fresh counts.
- #29's step-2 change must decide what to do with the ALGO-1 cliff above.
