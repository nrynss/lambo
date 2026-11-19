# 0.12.0 and 0.13.0 on the Metal rig: the re-pin gate

The Apple-silicon dogfood rig's run of the re-pin gate for the step from
0.12.0 to 0.13.0. The previous gate on this rig is
[0.8.0 and 0.12.0](../0.12.0-vs-0.8.0-metal/README.md); the method is the
same, less the names preview and the ack-window probe, which 0.13.0 gives no
reason to repeat.

- **When:** 2026-11-19.
- **Builds:** both are published release assets, `macos-arm64-metal`: 0.12.0
  (tag `v0.12.0`, 015d4acc, the rig's pinned binary) and 0.13.0 (tag
  `v0.13.0`, 76726f52), checksum-verified by `install.sh`.
- **Machine:** Apple M3 Pro, 18 GB, macOS 26.7.3. Every build embeds with
  candle BGE-M3 on Metal.
- **Store:** a SQLite backup of the serving store taken while the writer was
  live: 7,438 concepts and 3,028 interactions across 8 sessions. The gate
  ran against the largest session, `lambo` (2,665 concepts, 1,025
  interactions), with the `general` layer attached for the HTTP runs as on
  the live rig. Every run used its own fresh copy, provisioned by the build
  that served it. The live writer stayed up on the same GPU throughout.
- **Provisioning:** 0.13.0 makes no schema change.

These files hold aggregates only, with no memory content and no local paths.
The replay file's three path fields (the two binaries and the snapshot) are
reduced to their file names; nothing else in any file is edited.

## What 0.13.0 changes

Both changes are on the recall side.

- Every recall reports `top_legs`, a provisional `confidence` and
  `layer_top_legs` (#256). The fields are additive.
- When a dependency question's structural dispatch declines under the graph
  lock, the daemon runs the vector search it had skipped and retries, where
  0.12.0 answered from the keyword leg with a warning (#263).

The write path is not changed, which the replay below is the check on.

## Summary

| Test | 0.12.0 | 0.13.0 | Verdict |
|---|---:|---:|---|
| Replay of 330 real writes | 330 applied | 330 applied | PASS, 0 unexpected, matching final store counts |
| Top-8 recall on the 20 A/B queries | | | identical on 19; the other is a startup-timing artefact (section 2) |
| Sequential recall p50 / p99 | 2.02 / 21.69 ms | 2.02 / 21.66 ms | within 0.1 ms, one run |
| Sequential `lambo_stats` p50 | 0.43 ms | 0.39 ms | within 0.05 ms, one run |
| Sequential derive ack p50 / p90 | 0.52 / 0.58 ms | 0.52 / 0.56 ms | within 0.05 ms, one run |
| Sequential derive applied p50 / p90 | 25.94 / 26.29 ms | 25.92 / 26.18 ms | within 0.2 ms, one run |
| 8 agents: writes failed | 0 / 543 | 0 / 543 | none failed on either |
| 8 agents: recall p50 / p99 | 10.41 / 33.82 ms | 10.55 / 38.52 ms | close at p50, 0.13.0 higher at p99, one run each |
| 8 agents: derive ack p50 | 2.43 ms | 2.46 ms | close, one run each |
| 8 agents: apply p50 / p90 (server) | 54 / 65 ms | 53 / 63 ms | close, one run each |
| 16 agents: writes acked / failed | 2,128 / 0 | 2,128 / 0 | none failed on either |
| 16 agents: recall p50 / p99 | 5.71 / 28.85 ms | 5.82 / 28.50 ms | close, one run each |
| 16 agents: derive ack p50 | 0.79 ms | 0.77 ms | close, one run each |
| 16 agents: apply p50 / p90 (server) | 57 / 109 ms | 53 / 104 ms | close, one run each |

**These runs show no regression and no speed-up.** The sequential figures of
the two builds are within 0.2 ms of each other. The concurrency figures are
close; the largest gap is the 8-agent recall p99, 4.7 ms higher on 0.13.0,
while the 16-agent p99 is 0.35 ms lower. Every cell is one run, so these
runs do not establish whether a difference of that size is noise.

The rig was re-pinned to 0.13.0 the same day (DOGFOOD-SETUP §2a, all 8
sessions re-attached). After the swap the serve
ledger records `top_legs` on each recall.

## 1. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`, the last 300 real write
calls of session `lambo` (124 derive, 176 record_action) plus 30 reworded
variants, with `--canon-eval-secs 100000000 --canon-wait 0`. The built-in
reason catalogue needed no new entries.

File: [`replay-0120-0130.json`](replay-0120-0130.json).

Both sides applied 330 of 330, created 610 concepts, semantically merged 30,
and ended on stores with matching counts: 5,872 concepts, all embedded,
14,529 edges with the same count per edge type. The stores are not
identical: 13 semantic edges exist only on the 0.12.0 side and 13 only on
the 0.13.0 side, which is the merge-target difference of earlier gates.
32 calls differ, all `merge_into_replayed_concept`; 0 are unexpected.

## 2. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side,
10 rounds of 20 queries interleaved per query, then 30 derives per side each
followed by a receipt wait. 0.12.0 is listed first. Both serves ran with the
default rate limit; the harness retried one rate-limited request on the
0.12.0 side.

File: [`sequential-ab-0120-0130.json`](sequential-ab-0120-0130.json).

**One of the 20 queries returned a different top-8 set on the two builds**
(`recall_topk_jaccard` mean 0.98, where the previous gate read 1.0). It is
not a build difference. For a few seconds after a serve starts, recall
carries a "Session inactive" notice that forces one extra hit, and the
0.13.0 serve started two seconds after the 0.12.0 one, so one early query
fell inside that window on one side only. Re-run by hand with the 0.13.0
serve started first and the repeats spaced 2 s apart, the two builds
returned the same hits on all ten repeats. That re-run has no file here.

## 3. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s, default mix,
seed 1; 8 workers with 1.0 s think time and 16 with 0.5 s. One serve at a
time, each on a fresh copy, default rate limit. Every run drained, every
call succeeded (939 of 939 at 8 agents and 3,723 of 3,723 at 16, on both
builds) and no worker died. At 16 agents 2,000 of the acked receipts were
queried: that is the harness's `--receipt-max` sample cap
(`"sampled": true`), not lost writes.

Files: `conc-<build>-w<N>.json`.

## Not covered

- The #263 retry path. It fires only when a write lands between a recall's
  pre-check and its graph lock, which these harnesses do not provoke; the
  daemon tests cover it.
- The CUDA rig. This gate ran on the Metal rig alone.
