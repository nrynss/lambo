# 0.5.0 against 0.4.0: the full comparison suite

The suite that compared [0.4.0 with 0.3.0](../0.4.0-vs-0.3.0-load/README.md),
run again for 0.5.0, with one test added for #31.

- **When:** 2026-10-12.
- **Builds:** the published `macos-arm64-metal` release assets of both versions.
- **Machine:** Apple M3 Pro, 18 GB. Both builds embed with candle BGE-M3 on Metal.
- **Store:** every run used its own scratch copy of the dogfood store taken just
  before the 0.5.0 re-pin (5,005 concepts). The live writer was not touched.
- **Order:** the tests ran one after another on the same GPU, with the live writer
  resident and idle.

These files hold aggregates only, with no memory content. Local paths are replaced
with `<SCRATCH>`, `<RUNTIME>`, `<TMP>` and `~`.

## Summary

| Test | 0.4.0 | 0.5.0 | Verdict |
|---|---|---|---|
| 1. Sequential latency, recall p50 / p99 | 5.3 / 25.4 ms | 5.3 / 25.8 ms | same |
| 1. Sequential latency, derive ack then applied p50 | 0.59 / 32.3 ms | 0.53 / 28.1 ms | same, applied slightly faster |
| 1. Top-8 recall overlap between builds | 1.0 on all 20 queries | | identical results |
| 2. Unseen-query recall p50 (stdio) | 25.9 ms | 26.2 ms | same |
| 2. Server ready (stdio, holder) | 3.4 s | 0.8 s | **4× faster start** (cached weight identity, #31) |
| 3. Concurrent, 8 agents: writes failed | 253 of 519 | 0 of 521 | **fixed** (#116) |
| 3. Concurrent, 16 agents: writes failed + dropped | 740 + 701 of 2,009 | 0 + 0 of 2,003 | **fixed** |
| 3. Concurrent, apply p50 at 8 / 16 agents | 26 s / 323 s | 95 ms / 10.2 s | **much faster** (#125 tracks 16 agents) |
| 4. Replay of 330 real writes (canonization held off) | | | **identical**: same differing calls as 0.4.0 against itself |
| 5. Crash, SIGTERM | durable | durable | same |
| 5. Crash, SIGKILL: acked writes lost | 20 | 12 | same behaviour (#117, by design) |
| 6. Proxy footprint after election | 90 MB | 6.5 MB | **14× smaller** (#31) |

## 1. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py`. Two HTTP serves ran side by side, each
on its own store copy, and calls were sent one at a time.
- **Recalls:** 10 rounds × 20 fixed topic queries, interleaved between the builds
  per query.
- **Derives:** 30, each followed by a receipt wait.

File: [`sequential-ab.json`](sequential-ab.json).

**Results:**
- **Single-call latency:** unchanged. 0.5.0 did not touch the recall path, and
  the commit path is faster but does not dominate a single uncontended derive.
- **Recall results:** identical, top-8 Jaccard 1.0.
- **Memory:** both processes had a phys_footprint of about 1.58 GB with the model
  loaded. RSS (393 MB against 1,352 MB) is not comparable on Apple silicon,
  because it depends on when the Metal buffers were last touched.

## 2. Recall on unseen queries, and startup

**Method:** stdio serves answered 75 distinct real queries from the live ledger,
each asked once, after a 5-query warm-up.

File: [`unseen-recall-stdio.txt`](unseen-recall-stdio.txt).

**Results:**
- **Recall:** the same within noise.
- **Startup:** "Server ready" counts the time to the first `initialize` reply,
  including the model load. It fell from 3.4 s to 0.8 s, because 0.5.0 caches
  the weight file's identity instead of hashing 1.1 GB on every start.

## 3. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s.
- **Workload:** a mix of recall, derive, record_action and stats in the ratio
  40/38/18/4.
- **Load levels:** 8 workers with a 1.0 s think time, and 16 workers with
  0.5 s.
- **Receipts:** every receipt was queried after the queue drained.
- **Server counters:** the `final_stats` columns come from the serve itself.

Files: `concurrent-<build>-w<N>.json`.

| | 0.4.0, 8 | 0.5.0, 8 | 0.4.0, 16 | 0.5.0, 16 |
|---|---:|---:|---:|---:|
| writes acked | 519 | 521 | 2,009 | 2,003 |
| applied | 266 | **521** | 568 | **2,003** |
| failed (replan exhausted) | 253 | **0** | 740 | **0** |
| dropped (queue full) | 0 | 0 | 701 | 0 |
| apply p50 / p90 | 26.2 / 40.7 s | **95 / 139 ms** | 323 / 369 s | **10.2 / 15.5 s** |
| drain after load stopped | 32 s | 0 s | 342 s | 8 s |
| recall p50 / p99 | 6.0 / 370 ms | 21.3 / 142 ms | 5.8 / 597 ms | 12.4 / 578 ms |

- **0.4.0 at 16 agents:** its drain outlasted the 300 s receipt retention, so
  its receipt queries returned errors. Its counts come from the server's
  `final_stats`.
- **Recall median under load is higher on 0.5.0**, while the tail is the same or
  better. 0.5.0 applies every write, so recall shares the GPU with about twice
  as much embedding work. 0.4.0 failed half its writes, which left the GPU idle
  for recall. This is a consequence of doing the work, not a slower recall path:
  test 1 shows single-call recall unchanged.
- **Apply latency at 16 agents** is 10 s at p50 on 0.5.0, down from 323 s on
  0.4.0. Making it fast is #125.

## 4. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`.
- **Calls:** the last 300 real write calls plus 30 reworded variants.
- **Stores:** each replay starts from a copy of the store rewound to before those
  calls.
- **Canonization:** the timer is held off with
  `--canon-eval-secs 100000000 --canon-wait 0`, so the comparison does not
  depend on when canonization passes run.
- **Control:** 0.4.0 replayed against itself.

Files: [`replay-control.json`](replay-control.json),
[`replay-compare.json`](replay-compare.json).

| | 0.4.0 against 0.4.0 | 0.4.0 against 0.5.0 |
|---|---|---|
| verdict | PASS | PASS |
| applied / refused | 327 / 3 on both sides | 327 / 3 on both sides |
| created / semantically merged | 774 / 45 on both | 774 / 45 on both |
| final concepts / edges / embedded | 5,030 / 12,273 / 5,030 on both | the same on both |
| differing calls | 48, all a different semantic-merge target | 48, all a different semantic-merge target |

0.5.0's write outcomes cannot be told apart from 0.4.0's. The only differences
are the merge-target choices that also differ between two runs of 0.4.0.

## 5. Crash recovery

**Method:** [`capture_kill.sh`](../0.4.0-vs-0.3.0-load/harness/capture_kill.sh).
- **Load:** 12 workers through the `mcp_load.py` phases, with the signal sent
  5 s into the record_action burst.
- **SIGKILL runs:** the store is checked as the crash left it, then again after
  a restart that waits out the 45 s lease and replays durable intents for 60 s.

| | 0.4.0 | 0.5.0 |
|---|---|---|
| SIGTERM: signal to exit | 2.1 s, durable | 2.2 s, durable |
| SIGKILL: acked interactions missing | 20 | 12 |
| SIGKILL: durable intents replayed on restart | 550 | 152 |

- **What a SIGKILL loses** is unchanged: the writes acknowledged during the last
  flush interval (#117). The docs now say so.
- **Replay backlog:** 0.5.0 left about a quarter of the backlog for the restart
  to replay, because it applies writes as fast as they arrive.

## 6. Proxy footprint (#31)

**Method:** a holder serve took the session, then a second stdio serve on the same
session became its proxy. Its `phys_footprint` was read 25 s later.

File: [`proxy-footprint.txt`](proxy-footprint.txt).

| | 0.4.0 | 0.5.0 |
|---|---:|---:|
| proxy phys_footprint | 90 MB | **6.5 MB** |
| holder phys_footprint | 1.46 GB | 1.50 GB |

0.4.0 already released the model once the proxy learned its role (#13), so 25 s
in, it holds 90 MB of what is left over. During the election itself it held the
full model, about 1.4 GB. 0.5.0 never loads it.

## Not covered

**Live-ledger comparison.** It needs a few days of real 0.5.0 traffic on the
rig. Run it then with `mcp_ab.py ledger --switch 2026-10-12T01:56:20Z`.
