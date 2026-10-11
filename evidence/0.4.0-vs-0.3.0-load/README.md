# 0.4.0 against 0.3.0: concurrency, crash recovery and write parity

Three tests run on 2026-10-11, following the latency A/B in
[`../issue-108-perf/`](../issue-108-perf/README.md). That A/B sent one call at a time.
These tests cover what it could not: many agents at once, a crash in the middle
of writes, and whether the same writes produce the same graph.

Rig: Apple M3 Pro, 18 GB. Both builds are the `macos-arm64-metal` release assets,
running BGE-M3 through candle on Metal. Every run used a scratch copy of the dogfood store,
never the live writer.

The files hold aggregates only. No memory content is committed: the replay
reads concept text from the store, but only its counts are kept here.

| File | What it is |
|---|---|
| `concurrent-<build>-w<N>.json` | Test 1: per-tool latency, errors and write-queue counters for N concurrent agents |
| `durability-<build>-TERM.txt` | Test 3: `check_durability.py` after a SIGTERM in the middle of a burst |
| `durability-<build>-KILL-{before,after}-restart.txt` | Test 3: the same check after SIGKILL, before and after a restart that replays durable intents |
| `parity-summary.json` | Test 2: replay totals, final store counts, canonization counts, and the differing calls grouped by reason |
| `harness/` | The three drivers used. See each file's header for usage |

## Test 1: concurrent agents

The driver is [`harness/concurrent_mix.py`](harness/concurrent_mix.py), which reuses
`scripts/loadtest/mcp_ab.py`'s client. N workers each hold their own MCP session.
Between calls, each worker waits an exponentially distributed think time. Each
call is one of:

| Tool | Share |
|---|---:|
| `lambo_recall` (real ledger queries) | 40% |
| `lambo_derive` (two concepts) | 38% |
| `lambo_record_action` | 18% |
| `lambo_stats` | 4% |

That mix is the live ledger's. Each run lasts 120 s, then the write queue
drains. The two builds ran one after the other, each on a fresh store copy.

| Build, workers, think time | Calls | Recall p50 | Recall p90 | Recall p99 | Writes failed | Writes dropped |
|---|---:|---:|---:|---:|---:|---:|
| 0.3.0, 8, 1.0 s | 139 | 16,431 ms | 18,339 ms | 21,135 ms | 27 of 74 | 0 |
| 0.4.0, 8, 1.0 s | 894 | **7 ms** | 100 ms | 285 ms | 165 of 497 | 0 |
| 0.3.0, 16, 0.5 s | 151 | 47,041 ms | 63,184 ms | 64,890 ms | 53 of 94 | 0 |
| 0.4.0, 16, 0.5 s | 3,484 | **6 ms** | 16 ms | 587 ms | 670 of 1,993\* | 682\* |

\* A snapshot, not final totals. The driver's 300 s drain wait ran out (`drain_s`
  300.3 in the JSON) before this run's queue emptied, so the failed and dropped
  counts were read with writes still outstanding. Every other row drained
  fully (5 to 23 s).

- **Recall.** 0.3.0 serializes recall behind the graph scan. With 8 agents a recall
  waits 16 s, and with 16 agents 47 s, so agents spend almost the whole run
  waiting. This is the tail the live 0.3.0 writer showed: p99 5.7 s, max 13 s.
  0.4.0 keeps recall in milliseconds at both levels and serves 6 to 23 times
  as many calls.
- **Writes: a real defect in both builds, filed as #116.** About a third of
  writes fail under concurrent writers.
  - 0.3.0 fails mostly with `hybrid vector candidate lookup timed out`.
  - 0.4.0 fails only with `hybrid derive could not commit after 8 concurrent
    graph changes`. Hybrid derive commits only if the whole-graph epoch has not
    moved since it planned, and it re-embeds on every replan.
  - At 16 workers the 0.4.0 queue falls minutes behind: apply p50 271 s. It
    starts refusing at its bound, which is the `dropped` column.
  - Live dogfood load is far lighter: 1 failed completion in 1,628.
- **Derive and record_action acks** stay under 1 ms at p50 on both builds,
  because the write is applied after the ack.

## Test 2: replaying past writes

[`harness/replay_parity.py`](harness/replay_parity.py) rebuilds the last 300 real
write calls (206 derive, 94 record_action) from the pre-upgrade store. One
interaction is one call, and the concept text comes back from the
interaction's prompt. The script then:

1. rewinds a copy of the store to just before those calls (2026-10-08 15:04 UTC);
2. replays them in their original order into each build over stdio, waiting on
   every receipt;
3. adds 30 reworded variants ("Recap: ...") to exercise semantic merge.

| | 0.3.0 | 0.4.0 |
|---|---:|---:|
| applied / failed / refused | 329 / 1 / 0 | 326 / 1 / 3 |
| created / matched / semantically merged | 817 / 4 / 40 | 812 / 4 / 41 |
| final concepts / edges | 4,969 / 12,132 | 4,964 / 12,122 |
| concepts without a vector | 0 | 0 |
| Candidate / Venerable after the run | 428 / 14 | 427 / 14 |

140 of 330 calls differ. Each difference traces to a change listed in the 0.4.0
CHANGELOG:

- **94:** the record_action receipt now reports `embedded`. This changes the
  receipt only, not the store.
- **43:** semantic merges land on a different target. 0.4.0's SQLite session
  holder also ranks concepts written earlier in the run that are not flushed
  yet (#8).
- **3:** 0.4.0 refuses the derive. These are review notes about the #22 image
  suffix whose text quotes suffix-shaped tokens such as `[IMAGE:x]` and
  `[image:r17]s`. They normalize to a valid `[image:<id>]`, and 0.4.0 refuses
  that in derived text by design (#22 PR 4). These three account for the
  5-concept and 10-edge gap above.

## Test 3: crash in the middle of writes

The harness is [`harness/capture_kill.sh`](harness/capture_kill.sh), a copy of
`scripts/loadtest/capture_sigterm.sh` using the candle embedder. It runs 12 workers
through the `mcp_load.py` phases. The signal lands 5 s into the record_action
burst.

| Build, signal | Acked writes | Interactions missing from the store | Signal to exit |
|---|---:|---:|---:|
| 0.3.0, SIGTERM | 1,132 | 0 | 2.1 s |
| 0.4.0, SIGTERM | 1,132 | 0 | 2.1 s |
| 0.3.0, SIGKILL | 1,135 | **10** | 26 ms |
| 0.4.0, SIGKILL | 1,136 | **19** | 24 ms |

- **A clean shutdown is durable on both builds.**
- **SIGKILL loses the last fraction of a second of acked writes, on both builds.**
  Filed as #117.
  - The lost writes all fall in the final second before the kill: 10 of 38 acks
    on 0.3.0 and 19 of 44 on 0.4.0.
  - The cause is that the intent is recorded through the 1 s write-behind
    flush, so the ack does not wait for disk.
  - `docs/reference/mcp.mdx` says an acked write is durable at admission. That is
    true for a clean close and not for a crash.
- **Restart replays every intent that reached disk:** 606 on 0.3.0 and 561 on
  0.4.0, one at a time in admission order. The 20 s restart window shown in the
  after-restart files was not enough to finish the backlog. Replay runs at
  embedder speed, about 5 writes/s here.
- **A restart inside `LEASE_TTL` (45 s) is refused.** That is correct, but the
  message says the dead holder "is still refreshing" the lease. Filed as #118.
