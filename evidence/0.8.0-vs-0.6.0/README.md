# 0.6.0, 0.7.0 and 0.8.0: the re-pin gate

The checks the dogfood rig runs before a re-pin, run three ways this time
because the rig skipped 0.7.0: it went from 0.6.0 straight to 0.8.0. Same gate
as [0.6.0 vs 0.5.0](../0.6.0-vs-0.5.0/README.md), plus a one-shot recall
overlap check across all three builds.

- **When:** 2026-10-28.
- **Builds:** 0.6.0 is the rig's pinned binary (tag `v0.6.0`, 75ab5862). 0.7.0 is
  tag `v0.7.0` (21942f6f) and 0.8.0 is tag `v0.8.0` (15585529). All three are
  built with `--features store-sqlite,embed-candle-cuda`, since the Linux release
  assets carry no candle.
- **Machine:** Linux x86_64, RTX 4070 SUPER (12 GB), CUDA 13.4. Every build
  embeds with candle BGE-M3 on CUDA.
- **Store:** every run used its own copy of a backup of the dogfood store taken
  just before the gate (9,746 concepts, 3,200 interactions). The live writer
  stayed up and idle on the same GPU and was not touched.
- **Provisioning:** each copy was provisioned by the build that served it. On
  0.6.0 that is a no-op. On 0.7.0 and 0.8.0 it transcodes vectors to packed
  `f32` (#55) and re-keys **3,074 identifier concepts** (#148). It recorded
  `canonical_key_rule` in `store_meta`, which happens only when the re-key leaves
  no collision.

These files hold aggregates only, with no memory content and no local paths.

## Summary

| Test | 0.6.0 | 0.7.0 | 0.8.0 | Verdict |
|---|---:|---:|---:|---|
| Replay of 330 real writes, against 0.8.0 | PASS | PASS | | no unexpected difference |
| Top-8 recall on the 20 A/B queries | | | | identical on all 20, all three builds |
| Sequential recall p50 / p99 (sec. 4) | 18.5 / 306.4 ms | 10.6 / 49.8 ms | 11.0 / 44.7, 10.7 / 58.3 ms | 0.7.0+ faster, no tail |
| Sequential derive applied p50 (sec. 4) | 55.2 ms | 47.2 ms | 39.5, 59.9 ms | noisy, one run per pair |
| 8 agents: writes failed | 0 / 534 | 0 / 545 | 0 / 543 | same |
| 8 agents: recall p50 / p99 | 23.8 / 322.8 ms | 12.5 / 78.4 ms | 12.9 / 70.7 ms | **0.7.0+ faster** |
| 8 agents: derive ack p50 | 1.7 ms | 0.9 ms | 1.0 ms | about the same |
| 8 agents: apply p50 / p90 (server) | 74 / 141 ms | 54 / 97 ms | 53 / 94 ms | 0.7.0+ faster |
| 16 agents: writes acked / failed | 1,651 / 0 | 2,001 / 0 | 1,982 / 0 | 0.7.0+ gets ~20% more through |
| 16 agents: recall p50 / p99 | 191.3 / 945.5 ms | 43.2 / 132.8 ms | 49.9 / 135.0 ms | **0.7.0+ 4-7x faster** |
| 16 agents: derive ack p50 | 88.3 ms | 38.3 ms | 43.5 ms | 0.7.0+ faster |
| 16 agents: apply p50 / p90 (server) | 157 / 295 ms | 70 / 152 ms | 113 / 179 ms | 0.8.0 slower than 0.7.0, still faster than 0.6.0 |
| Undeclared supersessions in the existing store (#198) | | | 0 pairs | nothing to backfill |

Nearly all the speed comes from 0.7.0 (#165, #171, #55): the recall p99 tail
from structural queries (#165) is gone, and the read path no longer queues
under contention. 0.8.0 keeps those gains. Apply p50 at 16 agents rises from
70 to 113 ms against 0.7.0. The likely cause is 0.8.0's extra write-path work:
merge guards (#193), the suspected-supersession check and replacement-candidate
listing (#198). This is one run per cell, so the size of that cost is
indicative only. It is still below 0.6.0, and no write failed or was dropped.

## 1. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`, the last 300 real write calls
(112 derive, 188 record_action) plus 30 reworded variants. Each side replays
from a copy rewound to before those calls. Canonization is held off with
`--canon-eval-secs 100000000 --canon-wait 0` and no `[daemon]` section. The
built-in reason catalogue was used and needed no new entries.

Files: [`replay-060-080.json`](replay-060-080.json),
[`replay-070-080.json`](replay-070-080.json).

| | 0.6.0 against 0.8.0 | 0.7.0 against 0.8.0 |
|---|---|---|
| verdict | PASS | PASS |
| applied | 330 / 330 on both | 330 / 330 on both |
| created | 605 on both | 605 on both |
| semantically merged | 30 / 28 | 30 / 28 |
| final concepts / embedded | 9,726 / 9,691 on both | the same on both |
| differing calls | 36: 35 merge into a replayed concept, 1 pre-T merge target | 40: 39 and 1, the same classes |
| unexpected | 0 | 0 |

The 0.5.0-against-itself control from the previous gate differed on 42 calls, all
merge-target noise, so 36 and 40 are within the noise floor. 0.8.0 merged 2
fewer concepts than both 0.6.0 and 0.7.0, which is consistent with the #193 identifier guard
refusing a similarity merge. No self-control was run this time.

## 2. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s with the default mix
(recall/derive/record_action/stats 40/38/18/4), seed 1. 8 workers with a 1.0 s
think time, and 16 with 0.5 s. Each run had its own fresh store copy and serve,
one serve at a time.

Files: `conc-<build>-w<N>.json`. 0.7.0 at 16 agents shows 2,000 receipts
queried out of 2,001 acked. That is the harness's `--receipt-max` sample cap
(`"sampled": true`), not a lost write.

## 3. Recall overlap

**Method:** each build ran the 20 fixed `mcp_ab.py` queries with one-shot
`lambo recall --top-k 8` on its own provisioned, unwritten copy. Every build
returned the same set of ids for every query (Jaccard 1.0), the same number of
hits shown (108), and no warning lines. 0.8.0's `session_activity` rescale (#180)
moves the printed scores, not the top-8 membership, on these queries.

## 4. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side, each
on its own fresh store copy, calls sent one at a time. 10 rounds of 20 fixed
queries, interleaved per query, then 30 derives, each followed by a receipt
wait. One run per pair. The first attempt failed: an unrelated `llama-server`
held 5.4 GB of the 12 GB GPU, and the second serve hit `CUDA_ERROR_OUT_OF_MEMORY`.
It was rerun once that process was stopped. The 0.8.0 side in both pairs is the
binary the rig now runs.

Files: [`sequential-ab-060-080.json`](sequential-ab-060-080.json),
[`sequential-ab-070-080.json`](sequential-ab-070-080.json).

| | 0.6.0 | 0.8.0 | 0.7.0 | 0.8.0 |
|---|---:|---:|---:|---:|
| recall p50 / p99 | 18.5 / 306.4 | 11.0 / 44.7 | 10.6 / 49.8 | 10.7 / 58.3 |
| derive ack p50 / p90 | 28.4 / 60.6 | 15.5 / 49.1 | 1.5 / 33.7 | 20.1 / 63.2 |
| derive applied p50 / p90 | 55.2 / 94.1 | 39.5 / 65.3 | 47.2 / 78.5 | 59.9 / 85.4 |
| errors | 0 | 0 | 0 | 0 |
| top-8 Jaccard | 1.0 on all 20 | | 1.0 on all 20 | |

- **Recall:** 0.8.0 matches 0.7.0, and the 0.6.0 p99 tail (#165, structural
  queries) is gone.
- **Derive:** against 0.7.0, 0.8.0's applied p50 is about 13 ms slower, which
  matches the write-path cost seen at 16 agents in section 2. The ack p50 gap
  (1.5 ms against 20.1 ms) is probably the post-settle window that
  `mcp_ab.py`'s interleaving exposes, as in
  [0.6.0 vs 0.5.0](../0.6.0-vs-0.5.0/README.md#1-sequential-latency-ab), rather
  than a cost per write. At 8 agents in section 2 the two builds ack in the
  same 1 ms. This was not probed further.

## Upgrade notes for the rig (0.6.0 to 0.8.0)

- 0.7.0 requires `lambo provision` (vector transcode, #148 re-key). 0.8.0 adds
  no schema. Stop the writer first, since provision refuses a live lease.
- **One-way:** after provision a 0.6.0 binary cannot read the store's vectors.
  Once 0.8.0 records a `SuspectedSupersedes` edge, 0.7.0 cannot load the
  session either. Rollback means restoring the backup.
- The first 0.8.0 writer attach pins the embedder canary (#176, default
  `warn`).
