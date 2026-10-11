# 0.5.0 acceptance: concurrent writers, commit cost, proxy footprint and replay parity

The acceptance runs behind the 0.5.0 CHANGELOG Highlights, run on 2026-10-12.
They check the fixes for #116 (concurrent hybrid derives failing), #102 (the
commit copying the graph under the write lock) and #31 (a proxying serve
loading the model), and that 0.5.0 writes what 0.4.0 wrote.

Rig: Apple M3 Pro, 18 GB, BGE-M3 through candle on Metal. 0.4.0 is the
`macos-arm64-metal` release asset. 0.5.0 is a local build of `main` at
`d8da77e5`; the commits between it and the 0.5.0 tag change only scripts, docs
and the version. Every run used a scratch copy of a dogfood store backup, never
the live writer: the concurrent runs a 4,949-concept copy, the replay an
earlier backup.

The files hold aggregates only. No store content, ledger or serve log is
committed.

| File | What it is |
|---|---|
| `concurrent-0.4.0-w8.json` | 0.4.0, 8 agents, think 1.0 s. See the note on its receipt states below |
| `concurrent-0.5.0-w8.json` | 0.5.0, 8 agents, think 1.0 s |
| `concurrent-0.5.0-w16.json` | 0.5.0, 16 agents, think 0.5 s |

## Concurrent agents

Driver: [`scripts/loadtest/mcp_concurrent.py`](../../scripts/loadtest/mcp_concurrent.py),
with `--secs 120 --seed 1` and the default mix of `lambo_recall` 40,
`lambo_derive` 38, `lambo_record_action` 18 and `lambo_stats` 4. Each run
started on a fresh store copy, and the write queue drained before the
counters were read (`drain.drained` is true in every file).

| Build, agents, think | Writes acked | Applied | Failed | Dropped | Apply p50 | Apply p90 | Apply max | Drain |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0.4.0, 8, 1.0 s | 519 | 270 | **249** | 0 | 25.1 s | 40.4 s | 49.7 s | 31.1 s |
| 0.5.0, 8, 1.0 s | 522 | **522** | 0 | 0 | 93 ms | 146 ms | 216 ms | 0 s |
| 0.5.0, 16, 0.5 s | 1,999 | **1,999** | 0 | 0 | 7.1 s | 12.3 s | 14.3 s | 5.1 s |

The counts come from `final_stats` (`write_queue_accepted`, `_applied`,
`_failed`, `_dropped`, `_apply_ms_*`), the server's own counters after the
drain. "Apply" is the time from a write's ack to the write being in the
graph.

- **#116 is fixed.** Every one of 0.4.0's 249 failures was
  `hybrid_replan_exhausted` in the serve's ledger
  (`writes.failed_reasons_server_ledger`), the "could not commit after 8
  concurrent graph changes" error. 0.5.0 failed none at 8 or 16 agents, and
  every acked write's receipt reads `applied`.
- **Apply latency at 16 agents is the next limit (#125).** Each derive still
  embeds its own items, one write at a time, so the embedder bounds the
  queue at about 5 writes/s (`write_queue_items_per_sec`). The queue keeps up
  and drains 5 s after load stops, but at 16 agents a write takes seconds to
  land.
- **Recall.** 0.5.0's recall at 8 agents is 20 ms at p50 and 52 ms at p90,
  against 0.4.0's 6 ms and 220 ms. The p50 rises while 0.5.0 applies twice as
  many writes through the same embedder.

**About `concurrent-0.4.0-w8.json`.** It was produced before the receipt
pacing fix in #124. All clients share one credential, and the driver then
queried every receipt back to back, so past the serve's rate-limit burst the
queries were refused with 429 and counted as `query_error`. That is why its
`writes.receipt_states` shows 414 `query_error` of 519, and why its
`writes.failure_rate` (45 failed of the 105 receipts that answered) is not
the build's failure rate. Read `final_stats` for the server-side counts
(`write_queue_failed` 249, `write_queue_applied` 270) and
`writes.failed_reasons_server_ledger` for the reason. The two 0.5.0 files
read every receipt.

## Commit write-lock hold (#102)

The commit used to copy the whole session graph under the write lock. It now
writes in place and undoes on failure. Measured by the ignored probe
`graph::hybrid::tests::commit_cost` (release build, a two-concept derive,
median of 21 runs; method in
[`dev-diary/notes/fix-116-102-derive-commit.md`](../../dev-diary/notes/fix-116-102-derive-commit.md)):

| Concepts | 0.4.0 (copy) | 0.5.0 (in place) |
|---:|---:|---:|
| 10,000 | 6.8 ms | 16.5 µs |
| 50,000 | 36.2 ms | 17 µs |
| 100,000 | 168 ms (from #102) | 21 µs |

## Proxy embedder footprint (#31)

Measured by the ignored probe `embed::candle::tests::metal_proxy_footprint`
(method in
[`dev-diary/notes/fix-31-proxy-defer-load.md`](../../dev-diary/notes/fix-31-proxy-defer-load.md)).
A serve that becomes a proxy now holds the candle embedder's identity and a
Metal device handle, about 4 MB, instead of the loaded model, about 1.4 GB.
With the weight digest cached, constructing the identity again takes under
1 ms instead of hashing the 1.1 GB weight file.

## Replay parity against 0.4.0

Driver: [`scripts/loadtest/replay_parity.py`](../../scripts/loadtest/replay_parity.py)
on a backup of the dogfood store, replaying its last 293 real write calls
(210 derives, 83 record_actions) plus 30 reworded variants through 0.4.0 and
0.5.0. The reason catalogue adds the 0.5.0 cycle refusal as an expected
difference.

- **Write outcomes are identical.** Every call has the same state on both
  builds, with the same created, matched, semantically merged, edge and
  embedded counts, and the final stores hold the same numbers of concepts,
  interactions and edges of each type.
- **One call 0.4.0 failed now applies.** In a window that reaches further
  back, 0.4.0 failed one large derive on its 30 s I/O deadline: it re-embedded
  every item on each replan. 0.5.0 reuses the vectors it has already
  embedded across replans, and the same call applies in 5 s.
- **Canonization counts are timing noise.** The daemon's canonization passes
  run on a timer, so how many concepts each pass promotes depends on where
  the passes fall relative to the writes. The counts vary on both builds: 0.4.0
  alone gave 401 to 433 canonization outcomes across timer settings. A 0.4.0-against-0.4.0 control
  run also flags about 45 calls whose semantic merge picked a different
  target concept, with identical counts. Neither is a difference between the
  builds, but both make the script's per-call verdict FAIL at the default
  settings.

**The timing-independent comparison.** Replay with canonization off, for
example `--canon-eval-secs 100000000 --canon-wait 0`, then compare the two
final stores directly and run canonization offline over each at a fixed
clock. Done that way, the 0.4.0 and 0.5.0 final stores are field-identical,
and the fixed-clock evaluation is byte-identical. Use this comparison, not
the per-call canonization counts, to judge a later release.

The parity outputs are not committed: their headers carry local paths, and the
timer-driven runs would be misleading without this context.
