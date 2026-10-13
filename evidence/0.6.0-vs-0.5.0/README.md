# 0.6.0 against 0.5.0: the re-pin gate

The checks the dogfood rig runs before a re-pin, for 0.5.0 to 0.6.0. This is
the gate subset of the [0.5.0 vs 0.4.0 suite](../0.5.0-vs-0.4.0/README.md):
replay parity, sequential latency and concurrent agents. Crash recovery,
unseen-query recall and proxy footprint were not rerun.

- **When:** 2026-10-13.
- **Builds:** 0.5.0 is the rig's pinned binary; 0.6.0 is tag `v0.6.0` (75ab5862).
  Both are built with `--features store-sqlite,embed-candle-cuda`. The Linux
  release assets carry no candle, so this rig builds from the tag.
- **Machine:** Linux x86_64, RTX 4070 SUPER (12 GB), CUDA 13.4. Both builds
  embed with candle BGE-M3 on CUDA.
- **Store:** every run used its own copy of a backup of the dogfood store taken
  just before the re-pin (9,064 concepts). The live writer stayed up and idle on
  the same GPU, and was not touched.
- **Provisioning:** 0.6.0 refuses a 0.5.0 SQLite store until `lambo provision`
  adds `reason` and `superseded_by` to `canonization_events`, so every copy was
  provisioned before either build attached, for both builds alike.

These files hold aggregates only, with no memory content and no local paths.

## Summary

| Test | 0.5.0 | 0.6.0 | Verdict |
|---|---|---|---|
| 1. Sequential latency, recall p50 | 17.5-17.7 ms | 17.6-18.0 ms | same |
| 1. Sequential latency, recall p99 | 23-51 ms | 288-311 ms | **slower**, one query class: [#165](https://github.com/nrynss/lambo/issues/165) |
| 1. Top-8 recall overlap between builds | 1.0 on all 20 queries | | identical results |
| 1. Derive ack / applied p50 (mcp_ab pattern) | 0.9-1.1 / 24-27 ms | 52-55 / 79-80 ms | see below: mostly a timing artifact |
| 2. Concurrent, 8 agents: writes failed | 0 of 540 | 0 of 533 | same |
| 2. Concurrent, 16 agents: writes failed | 0 of 1,825 | 0 of 1,763 | same |
| 2. Concurrent, apply p50 at 8 / 16 agents | 56 / 147 ms | 73 / 147 ms | about the same |
| 3. Replay of 330 real writes (canonization held off) | | | **identical**: fewer differing calls than 0.5.0 against itself |

0.6.0 writes exactly what 0.5.0 writes and recalls exactly what 0.5.0 recalls.
The one real regression is recall latency for queries with a structural marker
(`blast radius`, `depends on`, ...), about 260 ms each on this store, filed as
[#165](https://github.com/nrynss/lambo/issues/165) with its cause.

## 1. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side, each
on its own store copy, calls sent one at a time. 10 rounds of 20 fixed queries,
interleaved per query; then derives, each followed by a receipt wait. Run twice:
30 derives, then 60.

Files: [`sequential-ab-run1.json`](sequential-ab-run1.json),
[`sequential-ab-run2.json`](sequential-ab-run2.json).

| | 0.5.0 run 1 | 0.6.0 run 1 | 0.5.0 run 2 | 0.6.0 run 2 |
|---|---:|---:|---:|---:|
| recall p50 / p99 | 17.7 / 50.6 | 18.0 / 310.9 | 17.5 / 23.5 | 17.6 / 288.0 |
| derive ack p50 | 0.9 | 55.5 | 1.1 | 52.2 |
| derive applied p50 | 24.2 | 80.0 | 27.0 | 78.9 |
| top-8 Jaccard | 1.0 | | 1.0 | |

**Recall p99.** Follow-up probe: [`recall-probe.txt`](recall-probe.txt). All of
the tail is one query, `blast radius warning dependents`, at about 280 ms every
time on 0.6.0 against 21 ms on 0.5.0. The words `blast radius` alone reproduce
it, graph traversal depth makes no difference, and both builds return the same
hits. A structural marker sends the query to anchor resolution, which in 0.6.0
Unicode-normalizes every concept's text on every such query. Cause and fix
options are in [#165](https://github.com/nrynss/lambo/issues/165).

**Derive ack.** Follow-up probe: [`derive-ack-probe.txt`](derive-ack-probe.txt).
Sent idle or back to back, the ack costs the same on both builds (about 0.9 ms
and 0.4 ms). Both builds are slow when a derive arrives right after the
previous write settled: 52 ms on 0.5.0 and 68 ms on 0.6.0. `mcp_ab.py` puts
the other build's derive and wait between one side's receipt wait and its next
derive. 0.5.0's slow window appears to end inside that gap and 0.6.0's does
not. That is a longer post-settle window on 0.6.0, not a new per-write cost, and
it was not investigated further. Server-side apply time in test 3 is no worse
(p50 38 ms on 0.5.0, 32 ms on 0.6.0).

## 2. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s with the default mix
(recall/derive/record_action/stats 40/38/18/4), seed 1. 8 workers with a 1.0 s
think time and 16 with 0.5 s. Each run had its own fresh store copy and serve,
one serve at a time. Every receipt was queried after the queue drained.

Files: `concurrent-<build>-w<N>.json`.

| | 0.5.0, 8 | 0.6.0, 8 | 0.5.0, 16 | 0.6.0, 16 |
|---|---:|---:|---:|---:|
| writes acked | 540 | 533 | 1,825 | 1,763 |
| applied | 540 | 533 | 1,825 | 1,763 |
| failed / dropped | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| drain after load stopped | 0.5 s | 0.5 s | 0 s | 0 s |
| apply p50 / p90 (server) | 56 / 116 ms | 73 / 131 ms | 147 / 264 ms | 147 / 290 ms |
| recall p50 / p99 | 23.9 / 78.6 ms | 27.8 / 354.6 ms | 108.7 / 568.7 ms | 115.5 / 748.6 ms |

- **Writes:** no failures or drops on either build at either load.
- **Recall p99:** the driver draws its queries from the same list as test 1, so
  the higher 0.6.0 tail is the same structural-query cost (#165), not a
  contention effect.

## 3. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`.
- **Calls:** the last 300 real write calls (170 derive, 130 record_action) plus
  30 reworded variants.
- **Stores:** each replay starts from a copy rewound to before those calls.
- **Canonization:** held off with `--canon-eval-secs 100000000 --canon-wait 0`
  and a config template with no `[daemon]` section.
- **Control:** 0.5.0 replayed against itself.

Files: [`replay-control.json`](replay-control.json),
[`replay-compare.json`](replay-compare.json).

| | 0.5.0 against 0.5.0 | 0.5.0 against 0.6.0 |
|---|---|---|
| verdict | PASS | PASS |
| applied | 330 / 330 on both sides | 330 / 330 on both sides |
| created / semantically merged | 810 / 28 on both | 810 / 28 on both |
| final concepts / edges / embedded | 9,092 / 21,875 / 9,057 on both | the same on both |
| differing calls | 42, all a different semantic-merge target | 38, all a different semantic-merge target |

0.6.0's write outcomes cannot be told apart from 0.5.0's.

## Re-pin

After these checks the dogfood writer and portal were re-pinned to 0.6.0 the
same day: backup, stop, `lambo provision`, restart. Canonization status counts
were unchanged by the provision.

The 0.6.0 CHANGELOG says SQLite converges the two new `canonization_events`
columns "through its column guard on attach". In practice the guard refuses an
unprovisioned store and names `lambo provision`. Upgrading a SQLite store from
0.5.0 needs that step, with the writer stopped.
