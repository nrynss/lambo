# 0.13.0 and 0.14.0 on the Metal rig: the re-pin gate

The Apple-silicon dogfood rig's run of the re-pin gate for the step from
0.13.0 to 0.14.0. The previous gate on this rig is
[0.12.0 and 0.13.0](../0.13.0-vs-0.12.0-metal/README.md). The method is the
same, with three additions: a wait before the sequential A/B, a per-query
look at recall with a same-build control (section 2), and follow-up runs at
16 agents after the first pass showed a latency tail on 0.14.0 (section 4).

- **When:** 2026-11-20. Times below are UTC.
- **Builds:** both are published release assets, `macos-arm64-metal`: 0.13.0
  (tag `v0.13.0`, 76726f52, the rig's pinned binary) and 0.14.0 (tag
  `v0.14.0`, 11074e42), checksum-verified by `install.sh`.
- **Machine:** Apple M3 Pro, 18 GB, macOS 26.7.3. Every build embeds with
  candle BGE-M3 on Metal. The machine was in desktop use throughout.
- **Store:** a SQLite backup of the serving store taken while the writer was
  live: 7,586 concepts and 3,109 interactions across 8 sessions. The gate
  ran against the largest session, `lambo` (2,743 concepts, 1,077
  interactions), with the `general` layer attached for the HTTP runs as on
  the live rig. Every run used its own fresh copy, provisioned by the build
  that served it. The live 0.13.0 writer stayed up on the same GPU
  throughout. The two totals follow from the replay file (`store.base` plus
  `meta.truncation`); the session count and the `lambo` counts were read
  from the snapshot and are not in a file here.
- **Provisioning:** `provision` by 0.14.0 left the SQLite schema text
  unchanged (compared by hand on a copy; not in a file here).

These files hold no query text and no concept text from the store, and no
local paths. The only warning lines are three `Session inactive N seconds`
lines in `probe-recall.txt`.

- The harness files (`sequential-ab-*.json`, `conc*.json`, `pair*.json`) and
  `probe-recall.txt` are as captured and hold aggregates, except that
  `probe-recall.txt` lists the hits of two queries by node id prefix, score,
  concept type and session.
- `replay-0130-0140.json` has one row per replayed call: its kind, outcome
  counts and timings, and for the 300 real calls the id of the agent that
  made the original write and the interaction's id, both as stored. Its
  three path fields (the two binaries and the snapshot) are reduced to their
  file names; nothing else in it is edited.
- `ledger-summary.json`, `ab-ledger-top8.json` and `probe-top8.json` are
  computed by the scripts beside them, from each serve's own call ledger and
  from the probe's saved recall bodies. Those inputs hold memory text and
  stay on the rig. The two top-8 files have one row per query and round,
  with counts, id prefixes and scores.

## What 0.14.0 changes

Five changes run inside a serving writer:

- `lambo_stats`, session attach and the ledger heartbeat check for text
  concepts that have had no vector for 60 seconds, and warn (#249).
- Each recall ledger line records `concept_count` and
  `layer_concept_counts` (#257).
- The `concept_type` enums are inlined in the MCP tool schemas and refusals
  list the accepted values (#250).
- When a layered session attaches, serve warns if calibration is off and
  the uncalibrated fallback floor is above the calibrated one (#260).
- `lambo_inspect` results carry `focus.concept_type` and `focus.resolution`
  (#251). No harness of this gate calls `lambo_inspect`.

The rest is outside the serve loop: `lambo stats --store` and
`--missing-vectors` (#248, #249), `lambo embed` and
`lambo recall --query-vector-as-text` (#258), and an offline script (#177).

## Summary

Latency rows are client-side, from the harness files, as in earlier gates.
The two rows marked (server) count slow calls and slow seconds from
`ledger-summary.json` and leave out each run's first 5 s (definition in
section 4).

| Test | 0.13.0 | 0.14.0 | Verdict |
|---|---:|---:|---|
| Replay of 300 real writes and 30 variants | 330 applied | 330 applied | PASS; 0 of 330 calls differ (section 1) |
| Top-8 recall on the 20 A/B queries | | | `recall_topk_jaccard` mean 0.95; the first eight hits are identical on all 200 query pairs, and the difference is a ninth hit on nine first-round queries (section 2) |
| Sequential recall p50 / p99 | 2.65 / 23.87 ms | 2.65 / 24.66 ms | within 0.8 ms, one run |
| Sequential `lambo_stats` p50 | 0.52 ms | 0.40 ms | 0.12 ms lower on 0.14.0, one run |
| Sequential derive ack p50 / p90 | 0.62 / 0.76 ms | 0.63 / 0.76 ms | within 0.01 ms, one run |
| Sequential derive applied p50 / p90 | 29.45 / 30.36 ms | 29.31 / 30.08 ms | within 0.3 ms, one run |
| 8 agents: writes failed | 0 / 545 | 0 / 546 | none failed on either |
| 8 agents: recall p50 / p99 | 8.70 / 41.87 ms | 8.57 / 36.86 ms | close at p50, 0.14.0 lower at p99, one run each |
| 8 agents: derive ack p50 | 1.74 ms | 1.38 ms | close, one run each |
| 8 agents: apply p50 / p90 (server) | 46 / 72 ms | 45 / 66 ms | close, one run each |
| 16 agents, first pass: writes acked / failed | 2,140 / 0 | 2,098 / 0 | none failed on either |
| 16 agents, first pass: recall p50 / p99 | 5.70 / 30.75 ms | 5.95 / 128.62 ms | p50 close; p99 higher on 0.14.0 in this run (section 4) |
| 16 agents, four single runs each: slow calls per run (server) | 0, 0, 5, 23 | 40, 91, 0, 6 | each build has runs with none and runs with a tail (section 4) |
| 16 agents, both builds at once: seconds with a call of 100 ms or more (server) | 6 and 13 | 7 and 15 | close; most fall within a second of one on the other build (section 4) |
| Writes failed, all 14 concurrent runs | 0 | 0 | none failed on either |

**These runs show no regression that can be attributed to 0.14.0, and they
do not show the two builds' tails to be equal.** Replay is identical (0 of
330 calls differ). The ranked recall hits are the same on both builds; the
A/B's 0.95 is a ninth notice hit on nine first-round queries (section 2).
The sequential figures in the table are within 0.8 ms of each other and the
8-agent run is close, one run each. At 16 agents the medians are close and a
tail of slow calls appears in some runs of both builds, more of it on 0.14.0
(94 slow calls over six runs on 0.13.0, 222 on 0.14.0). When both builds are
loaded at the same time their stalls mostly fall in the same seconds (18 of
19 and 17 of 22 slow seconds within a second of one on the other side). The
write, apply and embedder code is identical in the two tags, and the recall
and stats handlers gain constant work or fewer counting passes (section
4.4). What these runs do not show is in section 4.5.

The rig was re-pinned to 0.14.0 the same day (section 5).

## 1. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`, the last 300 real write
calls of session `lambo` (93 derive, 207 record_action) plus 30 reworded
variants, with `--canon-eval-secs 100000000 --canon-wait 0`. The built-in
reason catalogue needed no new entries.

File: [`replay-0130-0140.json`](replay-0130-0140.json).

**0 of 330 calls differ.** Both sides applied 330 of 330, created 574
concepts, semantically merged 29, and ended on stores with matching counts:
5,941 concepts, all embedded, 14,760 edges with the same count per edge
type. No created concept and no semantic edge exists on one side only. 36
calls carry reconstruction warnings.

## 2. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side,
10 rounds of 20 queries interleaved per query, then 30 derives per side each
followed by a receipt wait. 0.13.0 is listed first and was started first;
0.14.0 started about 4.5 s later (startup lines at 06:07:43 and 06:07:47 in
the two ledgers; not in a file here). Both serves ran with the default rate
limit; the harness retried one rate-limited request on the 0.13.0 side. The
script waited 90 s after both serves answered before starting the harness, a
wait the previous gate did not have (from the run log, not in a file here).

File: [`sequential-ab-0130-0140.json`](sequential-ab-0130-0140.json).

The latency figures are in the summary. **`recall_topk_jaccard` has mean
0.95 over the 20 queries**, where 1.0 means every query returned the same
set on both builds. The harness file carries the aggregate only. The
previous gate read 0.98 and left the cause unconfirmed; the wait did not
remove the difference.

### What the 0.95 is

Each A/B serve wrote its own call ledger, and a recall ledger line lists the
hits returned. [`ab-ledger-top8.json`](ab-ledger-top8.json), computed by
[`ab_ledger_top8.py`](ab_ledger_top8.py), pairs the two sides' 200 recalls.

- **The first eight hits are the same on both builds:** the same ids in the
  same order with a score difference of 0.0, on all 200 query pairs.
- **Every response has eight hits or nine.** The ninth is the same concept
  each time (id prefix 64c27e3d, score 0.3617), and a response has it
  exactly when its ledger line has `hot_warning` set.
- **The two serves stopped returning it at different points of the first
  round.** 0.13.0 returned it on queries 0 to 16 and 0.14.0 on queries 0 to
  7; neither returned it afterwards. `mcp_ab.py` scores each query on its
  first-round sets, so queries 8 to 16 score 8/9 and the mean is 0.95
  (`first_round_set_overlap_mean`).

So the 0.95 is nine queries on which 0.13.0 still returned a ninth hit and
0.14.0 no longer did. The file does not say why the two serves dropped it
about half a second apart (last ninth-hit recall at 06:09:25.3 and
06:09:24.9 by the ledgers; not in a file here). 0.13.0 had started 4.5 s
earlier and kept it longer.

### The same-build control

**Method:** [`probe_recall.py`](probe_recall.py) asks the same 20 queries,
with the same warm-up and `top_k = 8`, of three serves on fresh copies: two
instances of 0.13.0 and one of 0.14.0, started one after another in that
order, the last answering 17 s after the first. Three rounds; the script
waited 60 s after the last serve answered (start spread and wait from the
run log, not in a file here). It ran about 17 minutes after the A/B.

Files: [`probe-recall.txt`](probe-recall.txt), the probe's own report, and
[`probe-top8.json`](probe-top8.json), computed from its saved bodies by
[`probe_top8.py`](probe_top8.py).

- **The first eight hits are the same on all three serves:** the same ids in
  the same order with a score difference of 0.0, for every query in every
  round.
- **Every difference is the same ninth hit.** Nine of the 180 responses
  carry it (id prefix 64c27e3d, score 0.3617, against 3.96 or more for the
  ranked hits listed in the report), and each of those nine, and no other,
  carries a "Session inactive" warning.
- **It differs between two instances of the same build.** It appears only in
  the first round, on the first four queries: on all three serves for
  queries 0 and 1, on one 0.13.0 instance and on 0.14.0 for query 2, and on
  one 0.13.0 instance alone for query 3. Mean set overlap is 0.994 between
  0.13.0 and 0.14.0 and 0.989 between the two 0.13.0 instances.

Two instances of 0.13.0 disagree with each other in the same way the two
builds do: on when a serve stops returning the idle-session notice hit, not
on the ranked hits. The probe's differences are smaller than the A/B's (one
query between the builds, against nine). What decides the moment a serve
drops the notice was not traced in the source.

## 3. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s, default mix,
seed 1; 8 workers with 1.0 s think time and 16 with 0.5 s. One serve at a
time, each on a fresh copy, default rate limit, 0.13.0 first. Every run
drained, every call succeeded (943 of 943 and 944 of 944 at 8 agents; 3,745
of 3,745 and 3,681 of 3,681 at 16) and no worker died. At 16 agents 2,000 of
the acked receipts were queried: that is the harness's `--receipt-max`
sample cap (`"sampled": true`), not lost writes.

Files: `conc-<build>-w<N>.json`.

At 8 agents the builds are close on every row. At 16 agents the medians are
close and 0.14.0's tail is longer: recall p99 128.62 ms against 30.75 ms,
and server apply p50 / p90 of 90 / 345 ms against 52 / 96 ms. The server
figure covers only the most recent 256 applies (`APPLY_LATENCY_WINDOW` in
`src/writeq/counters.rs`), which in this run is the end of the load.

## 4. The 16-agent tail

The rest of the gate asks whether that tail belongs to 0.14.0. The figures
in this section are server-side, from each serve's own ledger
([`ledger-summary.json`](ledger-summary.json), computed by
[`ledger_summary.py`](ledger_summary.py), which states the definitions). A
**slow call** is a recall or a write ack of 100 ms or more that ended more
than 5 s after the run's first call; the first recalls of a fresh serve are
cold and can exceed that. In the tables, "N in M s" is N slow calls ending
in M distinct clock seconds, not a span of M seconds.

The runs in time order: first pass (06:14 and 06:17), second pass (06:28 and
06:33), the two pairs of section 4.3 (06:41 and 06:44), then four
alternating runs (06:48 to 06:58).

### 4.1 Where the tail sits in the first pass

The 0.14.0 run tracks the 0.13.0 run for its first 100 s and then has one
stall. In ten-second buckets from the first call (`conc-0140-w16`):

| From (s) | Load calls | Recall p50 (ms) | Recall max (ms) | Ack max (ms) | Apply lag max (ms) |
|---:|---:|---:|---:|---:|---:|
| 80 | 265 | 4.97 | 32.93 | 19.34 | 131.70 |
| 90 | 303 | 6.03 | 21.08 | 19.82 | 188.48 |
| 100 | 231 | 8.03 | 939.17 | 999.16 | 564.79 |
| 110 | 325 | 6.09 | 42.78 | 37.57 | 345.01 |

All 40 slow calls of that run ended between 06:19:26 and 06:19:29. Before
the 100 s bucket and after the first one, no recall took more than 32.93 ms
and no ack more than 42.53 ms. Over the whole run the server-side apply lag
is p50 52.15 and p90 112.70 ms, against 50.01 and 106.54 ms for 0.13.0.

### 4.2 Eight single runs

That stall fell at a moment taken to recur every 300 s on this machine, from
three times noted on the rig during the first pass (06:09:24, 06:14:22 and
this stall; not in a file here). The first two are the first second of the
A/B and of the first 16-agent run, where a first recall is cold. On that
basis the second pass started each build at the same point of a five-minute
cycle (first calls 06:28:24 and 06:33:25). The runs did not bear that out:
0.13.0 ran with no slow call, and 0.14.0 had slow calls in 25 separate
seconds between 06:33:51 and 06:35:17. Both passes had run 0.13.0 first, so
four more runs alternated the builds with 0.14.0 first. Same harness
arguments throughout (16 workers, think 0.5 s, 120 s, seed 1).

| First call | Build | Files | Recall p50 / p90 / p99 / max (ms) | Ack p99 / max (ms) | Apply lag p50 / p90 / p99 (ms) | Slow calls |
|---|---|---|---|---|---|---:|
| 06:14:22 | 0.13.0 | `conc-0130-w16` | 4.77 / 11.52 / 20.78 / 46.08 | 18.82 / 41.88 | 50.01 / 106.54 / 204.14 | 0 |
| 06:17:40 | 0.14.0 | `conc-0140-w16` | 4.90 / 12.41 / 111.35 / 939.17 | 114.72 / 999.16 | 52.15 / 112.70 / 404.59 | 40 in 4 s |
| 06:28:24 | 0.13.0 | `conc2-0130-w16` | 4.90 / 12.32 / 25.07 / 55.65 | 24.48 / 40.47 | 53.39 / 121.90 / 233.60 | 0 |
| 06:33:25 | 0.14.0 | `conc2-0140-w16` | 5.69 / 33.21 / 199.33 / 471.28 | 175.40 / 394.78 | 61.10 / 163.33 / 397.33 | 91 in 25 s |
| 06:48:52 | 0.14.0 | `conc3-r1-0140-w16` | 4.62 / 10.10 / 22.33 / 58.45 | 21.62 / 84.81 | 49.11 / 97.63 / 169.79 | 0 |
| 06:52:00 | 0.13.0 | `conc3-r2-0130-w16` | 4.71 / 13.17 / 32.85 / 192.06 | 29.56 / 184.29 | 49.72 / 104.86 / 200.01 | 5 in 1 s |
| 06:55:08 | 0.14.0 | `conc3-r3-0140-w16` | 4.94 / 16.73 / 40.95 / 126.75 | 32.88 / 125.51 | 51.57 / 105.08 / 181.15 | 6 in 1 s |
| 06:58:17 | 0.13.0 | `conc3-r4-0130-w16` | 5.33 / 20.77 / 102.89 / 186.24 | 57.69 / 177.40 | 55.90 / 123.46 / 257.79 | 23 in 5 s |

- Recall p50 is between 4.62 and 5.69 ms in all eight runs.
- Each build has a run with no slow call and a run with more than 20. Three
  runs have none: two of 0.13.0 (06:14:22 and 06:28:24) and one of 0.14.0
  (06:48:52). Of those the 0.13.0 run at 06:14:22 has the lowest recall and
  ack p99 (20.78 and 18.82 ms) and the 0.14.0 run the lowest apply lag (p99
  169.79 ms). The two runs with the longest tails are both 0.14.0 (06:17:40
  and 06:33:25).
- Within one build the count of slow calls ranges from 0 to 23 (0.13.0) and
  from 0 to 91 (0.14.0). Four runs a side do not separate the builds.
- No call failed and no write failed in any of the eight.

### 4.3 Both builds at the same time

**Method:** one serve of each build, each on its own fresh copy and port,
each under its own `mcp_concurrent.py` with the arguments above, started
together: the first calls are at 06:41:12 on both sides of pair 1 and at
06:44:38 on both sides of pair 2 (`first_call_utc`). Whatever else the
machine does reaches both at once. Two serves were under load at once, with
the live writer idle on the same GPU. Apply lag is about double that of a
single run (p50 101 to 110 ms against 49 to 61 ms); recall p50 is higher in
pair 1 only. Only the two sides of a pair compare. Pair 2 swaps the start
order and the ports.

Files: `pair<N>-<build>-w16.json`, and the `pairs` block of
`ledger-summary.json`, which compares the two sides second by second over
the 115 s after both have warmed up.

| | Pair 1: 0.13.0 | Pair 1: 0.14.0 | Pair 2: 0.13.0 | Pair 2: 0.14.0 |
|---|---:|---:|---:|---:|
| Started | first | second | second | first |
| Recall p50 / p99 (ms) | 7.17 / 78.80 | 7.24 / 94.41 | 4.96 / 96.49 | 5.08 / 122.87 |
| Ack p99 / max (ms) | 85.54 / 530.89 | 69.76 / 897.35 | 116.53 / 1,029.13 | 123.79 / 746.15 |
| Apply lag p50 / p90 / p99 (ms) | 104.82 / 282.96 / 616.49 | 110.37 / 374.34 / 2,537.36 | 103.65 / 333.44 / 753.47 | 100.82 / 331.04 / 680.72 |
| Slow calls | 25 in 6 s | 27 in 7 s | 41 in 13 s | 58 in 15 s |
| Seconds whose slowest call is 100 ms or more | 6 | 7 | 13 | 15 |
| Seconds whose slowest call is 500 ms or more | 1 | 1 | 2 | 2 |
| Slow seconds within 1 s of one on the other side | 6 of 6 | 5 of 7 | 12 of 13 | 12 of 15 |
| Seconds in which this side's slowest call is the slower by over 5 ms | 45 | 35 | 37 | 24 |

- **0.13.0 stalls too.** Its longest ack is 1,029.13 ms in pair 2, against
  746.15 ms for 0.14.0 beside it; in pair 1 the longer one is 0.14.0's,
  897.35 ms against 530.89 ms.
- **The stalls mostly coincide.** 18 of 0.13.0's 19 slow seconds and 17 of
  0.14.0's 22 are within a second of a slow second on the other side. The
  seconds at 500 ms or more are the same two seconds on both sides in pair
  2 and different seconds in pair 1.
- **Second by second, the slowest call does not lean towards 0.14.0 in
  these two pairs.** The median difference of the per-second slowest call,
  0.14.0 minus 0.13.0, is -0.44 ms in pair 1 and -0.81 ms in pair 2, and
  0.13.0's is the slower by over 5 ms in more of the 115 seconds in both.
- **The counts lean towards 0.14.0.** In both pairs the 0.14.0 side has more
  slow calls (27 against 25, 58 against 41), more slow seconds and the
  higher recall p99.
- **An apply-lag asymmetry in pair 1.** The 0.14.0 side's apply lag reached
  3,313 ms between 20 and 40 s into the load, where 0.13.0's stayed under
  870 ms (`buckets_10s`). In that pair the 0.14.0 serve was the one started
  second, and both serves were slow to come up before any load: 23 s and 20
  s from launch to first answer, where every other serve of the gate took 3
  to 6 s and one probe serve 14 s (from the run logs, not in a file here).
  Pair 2 does not show the mirror image for 0.13.0, so these two pairs do
  not say whether that is start order, the machine at that moment, chance,
  or the build.

### 4.4 The code a slow call runs through

In the first pass's stall, recalls and write acks are slow in the same
ten-second bucket (section 4.1). Between the two tags the write queue, the
background apply, the daemon, canonization, the flush, SQLite persistence,
the ledger writer, the candle embedder, recall candidate gathering and the
write and recall tool handlers do not differ. This prints nothing:

```sh
git diff --numstat v0.13.0..v0.14.0 -- src/writeq src/daemon src/canon \
  src/ledger.rs src/embed/candle.rs src/store/flush.rs \
  src/store/sqlite/persistence.rs src/memory/writes.rs src/memory/gate.rs \
  src/graph/hybrid src/graph/hybrid.rs src/recall/candidates.rs \
  src/mcp/server/tools/write.rs src/mcp/server/tools/recall.rs
```

The same command over `src/mcp/serve` reports a two-line comment change in
`registry.rs` and a new test file, and over `Cargo.toml` and `Cargo.lock`
one line each, the version.

Other files on a serve path did change (`git diff --stat v0.13.0..v0.14.0
-- src` lists them all). What they add to a call that the harness makes:

- A `lambo_stats` call in 0.13.0 took two readings of the session's
  figures, each counting concepts in three passes under the graph read
  lock; in 0.14.0 it takes one reading with one pass, which also gathers
  the #249 figures (`src/mcp/server/tools/stats.rs`,
  `src/mcp/server/stats.rs`, `src/memory/reads.rs`). Separately, its
  sequential p50 is 0.12 ms lower in the one A/B run.
- A recall adds `concept_count()`, a subtraction of two lengths
  (`src/graph/graph.rs`), once for the session and once per layer, and two
  fields on its ledger line (`src/mcp/server/trace.rs`).
- A derive parses `concept_type` with a hand-written match in place of the
  derived one (`src/mcp/server/params.rs`, #250).

Outside a call, session attach makes one new pass over the concepts for the
#249 check (`src/memory/builder.rs`).

### 4.5 What these runs do not show

- **The cause of the stalls.** Nothing here identifies it. Swap was nearly
  full when checked by hand after the runs (11.7 of 12.3 GB in use), which
  is not in a file here and was not sampled during them.
- **That the two tails are equal.** Six runs at 16 agents per build (four
  single, two paired) cannot show that, and what asymmetry there is leans
  one way: the two longest single-run tails are both 0.14.0, the 0.14.0
  side has more slow calls in both pairs, pair 1 has the apply-lag
  asymmetry, and over the six runs the slow calls total 94 on 0.13.0 and
  222 on 0.14.0.
- **Why both builds were clean or slow when they were.** The runs were not
  randomised. The one machine reading taken beside them is the 1-minute
  load average printed at the start of each of the four alternating runs:
  5.70, 3.90, 7.08 and 8.36 in run order (from the run log, not in a file
  here), where those runs had 0, 5, 6 and 23 slow calls. Each reading was
  taken just after the previous run stopped. Nothing was sampled during a
  run, and nothing was read for the other ten.

## 5. The re-pin

The rig was re-pinned to 0.14.0 at 07:22 the same day (DOGFOOD-SETUP §2a).
None of this section is in a file here; it is from the rig's ledger, log and
MCP answers.

- The first ledger heartbeat reported version 0.14.0 and git sha 11074e4.
- All 8 sessions re-attached, with every concept embedded (7,601 of 7,601)
  and no warning from the new #249 check.
- A write applied through the embedder, and a recall answered with a vector
  leg.
- The swap took about 45 s, where earlier re-pins took about 5. The 0.13.0
  writer's log ends in the first of its seven shutdown stages, five seconds
  after the signal, and its session leases were not released; the new writer
  answered 503 for each session until the leases expired. The process that
  did not finish shutting down was the 0.13.0 writer, so the delay is not
  0.14.0 code running. The shutdown path is also the same in both tags:
  `src/mcp/serve` differs by a comment and a test (section 4.4) and
  `src/memory/shutdown.rs` not at all. Why that shutdown did not finish was
  not traced.

## Not covered

- The #249 warning itself. Every harness file ends with all concepts
  embedded (`final_stats`), and no gate serve's log has the attach warning
  (checked on the rig; not in a file here). The check ran at attach and on
  each `lambo_stats`; the harness does not record whether a `lambo_stats`
  answer carried the warning.
- Ledger heartbeats under load. The gate serves ran with `--ledger` and
  without `--ledger-heartbeat`, which the live rig sets to 300 s, so
  neither build wrote a heartbeat during a run and the #249 check on the
  heartbeat first ran after the re-pin (section 5).
- `lambo_inspect` and its new focus fields (#251). No harness calls it: the
  14 concurrent runs made only `lambo_recall`, `lambo_derive`,
  `lambo_record_action` and `lambo_stats` calls (`calls_by_tool` in
  `ledger-summary.json`).
- The refusals that list the accepted `concept_type` values (#250). No
  harness write was refused (`refused_at_ack` is 0 in every `conc*.json` and
  `pair*.json`), and MCP clients other than the Python harness were not
  tried against the inlined schemas.
- The #260 warning. The gate's layer had its floor set (`min_similarity =
  0.50`), so the warning could not fire.
- `lambo embed`, `lambo recall --query-vector-as-text` (#258),
  `lambo stats --missing-vectors` and the offline script (#177).
  `lambo stats --store` was run once by hand against a copy (not in a file
  here).
- The cause of the stalls in section 4, and whether the graph lock that
  recalls and write acks share makes a short pause longer. Both builds have
  the same lock.
- The CUDA rig. This gate ran on the Metal rig alone.
