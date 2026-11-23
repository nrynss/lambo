# 0.14.0 and 0.15.0 on the Metal rig: the re-pin gate

The Apple-silicon dogfood rig's run of the re-pin gate for the step from
0.14.0 to 0.15.0. The previous gate on this rig is
[0.13.0 and 0.14.0](../0.14.0-vs-0.13.0-metal/README.md), and this one
follows its method: the same harnesses and arguments, the same-build recall
control, the paired and alternating 16-agent runs, and the same helper
scripts retargeted to the two new builds. Two timings are new (section 5):
loading the `lambo` session, which 0.15.0 checks for cycles in two passes
(#325), and a canonization dry run under both promotion policies, which
covers the Solo change (#300).

- **When:** 2026-11-22. Times below are UTC.
- **Builds:** both are published release assets, `macos-arm64-metal`: 0.14.0
  (tag `v0.14.0`, 11074e42, the rig's pinned binary until this gate) and
  0.15.0 (tag `v0.15.0`, merge commit 8f5fdd07). The 0.15.0 download was
  checked against its published `.sha256` (not in a file here).
- **Machine:** Apple M3 Pro, the same rig as the previous gate. Every build
  embeds with candle BGE-M3 on Metal. The live 0.14.0 writer stayed up on
  the same GPU throughout.
- **Store:** a SQLite `.backup` of the serving store taken at 11:32 while the
  writer was live: 7,732 concepts across 8 sessions. The gate ran against
  the largest session, `lambo` (2,743 concepts, 7,299 edges, 1,077
  interactions), with the `general` layer attached for the HTTP runs as on
  the live rig. Every run used its own fresh copy, provisioned by the build
  that served it. The total follows from the replay file (`store.base` plus
  `meta.truncation`) and the `lambo` counts from `stats-session-*.txt` and
  the canon dry-run reports; the session count is from the snapshot and not
  in a file here. The `lambo` session had no write between this snapshot
  and the previous gate's: its counts are the same as there, and its latest
  concept is from 2026-11-19 16:22 (from the snapshot, not in a file here).
- **Pending writes:** the snapshot's 10 `write_intents` rows were all
  applied, none owed (from the snapshot, not in a file here). The replay
  removed the 10 rows with the rest of the truncated history
  (`meta.truncation.write_intents`), and every concurrent run ends with
  `write_queue_replay_owed` and `write_queue_replayed` at 0 (`final_stats`).
- **Provisioning:** `provision` by 0.15.0 re-keyed 0 concepts and left the
  SQLite schema text unchanged; it moved the re-key marker in `store_meta`
  from `148-identifiers-unstemmed` to `301-empty-stems-dropped` (#301).
  0.14.0's provision reported the keys already followed its rule. From the
  run log and a comparison of the copies, not in a file here.

These files hold no query text and no concept text from the store, and no
local paths. The only warning lines are in `probe-recall.txt`: "Session
inactive N seconds" and "Load-bearing pillar" lines, which carry a count of
seconds or of dependent nodes and nothing else.

- The harness files (`sequential-ab-*.json`, `conc*.json`, `pair*.json`) and
  `probe-recall.txt` are as captured and hold aggregates, except that
  `probe-recall.txt` lists the hits of six queries by node id prefix, score,
  concept type and session.
- `replay-0140-0150.json` has one row per replayed call: its kind, outcome
  counts and timings, and for the 300 real calls the id of the agent that
  made the original write and the interaction's id, both as stored. Its
  three path fields (the two binaries and the snapshot) are reduced to their
  file names; nothing else in it is edited.
- `timing-stats-load.json` and `timing-canon-dryrun.json` are hyperfine
  exports. The paths in their `command` fields are reduced to file names
  (`--scratch timing` was the timing work directory); nothing else is
  edited. `stats-session-0140.txt` and `stats-session-0150.txt` are the
  output of the timed `lambo stats` command, one per build.
- `canon-dryrun-0140.json` and `canon-dryrun-0150.json` are the two
  `lambo canon dry-run --json` reports, with `source.path` reduced to its
  file name. They hold counts and full concept ids, and no text.
- `ledger-summary.json`, `ab-ledger-top8.json` and `probe-top8.json` are
  computed by the scripts beside them, from each serve's own call ledger and
  from the probe's saved recall bodies. Those inputs hold memory text and
  stay on the rig. The scripts are the previous gate's, with the build tags
  in their docstrings changed.

## What 0.15.0 changes

From the `## 0.15.0` section of `CHANGELOG.md`. Unlike the step to 0.14.0,
this release changes the write path, the flush loop, the daemon,
canonization and recall. Inside a serving writer:

- **Writes.** A write that meets an embedder outage in session is held and
  answers `pending_replay`, the agent's later writes wait behind it, and the
  worker retries the oldest on a backoff (#295). The replay at attach keeps
  each agent's writes in admission order and no longer trusts the wall
  clock for it (#295), and settles a durable intent the graph refuses
  instead of stopping (#294). A lane whose worker panicked gets a new
  worker. `write_queue_failed` no longer counts outages, and
  `write_queue_replay_blocked` is no longer `null` while intents are owed.
- **Lease and flush.** A writer whose heartbeat re-takes a lost lease is
  fenced, and a stale flush is terminal (#293). A store error no retry can
  fix degrades the session at once, a value the store rejects is
  dead-lettered, and the lease read and stats write after a failed flush run
  under the flush's timeout (#301).
- **Load at attach.** The load check tests `Causal`/`Dependency` acyclicity
  and `Hierarchical` acyclicity in two separate passes, so a session with a
  mixed cycle loads (#325).
- **Canonical keys.** An empty stem no longer reaches a key (#301).
- **Recall.** Zero-score ties under RRF and the default max fusion fall to
  the tie-break (#328); an exact durable-name match keeps its BM25 when that
  is above 1.0 (#288); a dependency-question phrase routes to traversal only
  as whole words (#299); hits carry durable names (#285).
- **Daemon and canonization.** Conflict detection keeps an edge's first
  writer (#297); the hot list is synced once per cycle (#298); Solo
  canonization dates a dependence by the turn that wrote it (#300);
  replacement candidates are listed at their highest cosine (#302). The rig
  runs the default Swarm policy: its `lambo.toml` has no
  `promotion_policy`, and its launchd job sets no environment (checked on
  the rig; not in a file here). The gate serves ran Swarm too.

The rest is outside the serve loop: store-wide session discovery in
`lambo serve-web` (#276, PR #287), `lambo re-embed` (#296, #322), the
Elasticsearch recall tier (#319; the rig has none), `--auth-token` parsing
(#315), the formal-model checks and property tests (#291), and the
observability scripts (#281).

## Summary

Latency rows are client-side, from the harness files, as in earlier gates.
Rows marked (server) are from `ledger-summary.json` and leave out each run's
first 5 s (definition in section 4).

| Test | 0.14.0 | 0.15.0 | Verdict |
|---|---:|---:|---|
| Replay of 300 real writes and 30 variants | 330 applied | 330 applied | PASS; 0 of 330 calls differ (section 1) |
| Top-8 recall on the 20 A/B queries | | | `recall_topk_jaccard` mean 0.98; the first eight hits are identical on all 200 query pairs, and the difference is a ninth hit on three first-round queries (section 2) |
| Same 20 queries, two 0.14.0 serves and one 0.15.0 | | | mean Jaccard 1.000 between the builds and 0.967 between the two 0.14.0 serves (section 2) |
| Sequential recall p50 / p99 | 2.13 / 24.53 ms | 2.09 / 23.33 ms | within 1.2 ms, one run |
| Sequential `lambo_stats` p50 | 0.40 ms | 0.36 ms | within 0.04 ms, one run |
| Sequential derive ack p50 / p90 | 0.55 / 0.58 ms | 0.55 / 0.57 ms | within 0.01 ms, one run |
| Sequential derive applied p50 / p90 | 25.59 / 26.03 ms | 25.62 / 26.10 ms | within 0.07 ms, one run |
| 8 agents: writes failed | 0 / 545 | 0 / 545 | none failed on either |
| 8 agents: recall p50 / p99 | 10.12 / 36.48 ms | 11.14 / 36.21 ms | p50 1.0 ms higher on 0.15.0, p99 close, one run each |
| 8 agents: derive ack p50 | 2.42 ms | 2.43 ms | close, one run each |
| 8 agents: apply p50 / p90 (server) | 53 / 63 ms | 52 / 66 ms | close, one run each |
| 16 agents, first pass: writes acked / failed | 2,129 / 0 | 2,130 / 0 | none failed on either |
| 16 agents, first pass: recall p50 / p99 | 5.78 / 24.56 ms | 5.84 / 39.26 ms | p50 close; p99 higher on 0.15.0 in this run, which has one stalled second (section 4) |
| 16 agents, three single runs each: slow calls per run (server) | 0, 0, 0 | 10, 0, 2 | 12 calls in two seconds on 0.15.0, none on 0.14.0 (section 4.1) |
| 16 agents, both builds at once: slow calls (server) | 0 and 1 | 0 and 0 | (section 4.2) |
| 16 agents, five runs each: recall p99 (server) | 20.22 to 25.25 ms | 21.71 to 29.93 ms | ranges overlap; 0.15.0 higher in both pairs (section 4.2) |
| Writes failed, all 12 concurrent runs | 0 | 0 | none failed on either |
| Session load, `lambo stats --session lambo`, 30 runs, mean | 149.3 ms | 145.2 ms | 4.1 ms lower on 0.15.0 (section 5) |
| Canonization dry run, 8 runs, mean | 2.123 s | 2.142 s | 0.9% higher on 0.15.0 (section 5) |
| Canonization dry-run reports | | | Swarm identical; Solo differs, 149 against 146 concepts reaching Candidate (section 5) |

**These runs show no difference in what 0.15.0 stores or returns, and no
difference in median latency. At 16 agents they show a small upper-tail
lean towards 0.15.0 that they do not resolve.** Replay is identical (0 of
330 calls differ). The ranked hits are the same on both builds, and the
probe's only differences are between two serves of 0.14.0 (section 2). The
sequential figures are within 1.2 ms of each other and the 8-agent run is
close, one run each. At 16 agents the medians are level and no write
failed, but over five runs a side 0.15.0 has 12 slow calls (10 of them in
one second of one run) against 1 on 0.14.0, a higher recall p99 in both
paired runs, and in both pairs more seconds in which its slowest call is
the slower by over 5 ms (section 4). Unlike the previous gate, the code a
slow call runs through did change between the tags (section 4.3), so these
runs cannot rule the build in or out. Session load is 2.7% faster on 0.15.0
and the canonization dry run 0.9% slower; under Swarm, the rig's policy, the
dry-run reports are identical (section 5).

The rig was re-pinned to 0.15.0 the same day (section 6).

## 1. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`, the last 300 real write
calls of session `lambo` (93 derive, 207 record_action) plus 30 reworded
variants, with `--canon-eval-secs 100000000 --canon-wait 0`. The built-in
reason catalogue needed no new entries. Because the `lambo` session had no
write between the two snapshots, these are the same 330 calls as the
previous gate's replay: the same cut-off (`meta.T`) and the same
interaction ids, agents and kinds in the same order (compare
[`../0.14.0-vs-0.13.0-metal/replay-0130-0140.json`](../0.14.0-vs-0.13.0-metal/replay-0130-0140.json)).

File: [`replay-0140-0150.json`](replay-0140-0150.json).

**0 of 330 calls differ.** Both sides applied 330 of 330, created 574
concepts, semantically merged 29, and ended on stores with matching counts:
5,941 concepts, all embedded, 14,760 edges with the same count per edge
type, and the same canonization status counts (545 Candidate, 16
Venerable). No created concept and no semantic edge exists on one side
only. 36 calls carry reconstruction warnings.

## 2. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side,
10 rounds of 20 queries interleaved per query, then 30 derives per side each
followed by a receipt wait. 0.14.0 is listed first and was started first;
0.15.0 started about 3 s later (startup lines at 12:05:15 and 12:05:18 in
the two ledgers; not in a file here). The script waited 90 s after both
serves answered before starting the harness, as in the previous gate. Both
serves ran with the default rate limit; the harness retried one
rate-limited request on the 0.14.0 side (`rate_limited_retries`).

File: [`sequential-ab-0140-0150.json`](sequential-ab-0140-0150.json).

The latency figures are in the summary. **`recall_topk_jaccard` has mean
0.98 over the 20 queries**, where 1.0 means every query returned the same
set on both builds. The harness file carries the aggregate only.

### What the 0.98 is

[`ab-ledger-top8.json`](ab-ledger-top8.json), computed by
[`ab_ledger_top8.py`](ab_ledger_top8.py) from the two serves' ledgers,
pairs the two sides' 200 recalls.

- **The first eight hits are the same on both builds:** the same ids in the
  same order with a score difference of 0.0, on all 200 query pairs.
- **Every response has eight hits or nine.** The ninth is the same concept
  each time (id prefix 64c27e3d, score 0.3617), and a response has it
  exactly when its ledger line has `hot_warning` set. It is the same
  concept as in the previous gate.
- **The two serves stopped returning it at different points of the first
  round.** 0.14.0 returned it on queries 0 to 9 and 0.15.0 on queries 0 to
  12; neither returned it afterwards. `mcp_ab.py` scores each query on its
  first-round sets, so queries 10 to 12 score 8/9 and the mean is 0.98
  (`first_round_set_overlap_mean` 0.9833).

So the 0.98 is three queries on which 0.15.0 still returned the ninth hit
and 0.14.0 no longer did. The two serves' last ninth-hit recalls are about
0.16 s apart (12:06:51.01 and 12:06:51.17 by the ledgers; not in a file
here). In the previous gate the serve started first kept the hit longer;
here the serve started second did.

### The same-build control

**Method:** [`probe_recall.py`](probe_recall.py) asks the same 20 queries,
with the same warm-up and `top_k = 8`, of three serves on fresh copies: two
instances of 0.14.0 (`0140a`, `0140b`) and one of 0.15.0, started one after
another in that order and answering at 12:38:46, 12:38:48 and 12:38:50.
Three rounds; the script waited 60 s after the last serve answered (start
times and wait from the run log, not in a file here). It ran after the
16-agent runs and about 33 minutes after the A/B.

Files: [`probe-recall.txt`](probe-recall.txt), the probe's own report, and
[`probe-top8.json`](probe-top8.json), computed from its saved bodies by
[`probe_top8.py`](probe_top8.py).

- **The first eight hits are the same on all three serves:** the same ids in
  the same order with a score difference of 0.0, for every query in every
  round.
- **0.15.0 and the first 0.14.0 serve return the same sets everywhere:**
  mean Jaccard 1.000.
- **Every difference is the same ninth hit, between the two 0.14.0
  serves.** 27 of the 180 responses carry it (id prefix 64c27e3d, score
  0.3617), each of those 27 and no other carries a "Session inactive"
  warning, and all are in the first round. All three serves return it on
  queries 0 to 4; `0140a` and 0.15.0 also return it on queries 5 to 10,
  where `0140b` does not. Mean Jaccard between the two 0.14.0 serves is
  0.967.

As in the previous gate, two instances of one build disagree in the same
way the A/B's two builds do: on when a serve stops returning the notice
hit, not on the ranked hits. Here the serve that stopped first was the one
started second of three. What decides that moment was not traced in the
source.

## 3. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s, default mix,
seed 1; 8 workers with 1.0 s think time and 16 with 0.5 s. One serve at a
time, each on a fresh copy, default rate limit, 0.14.0 first. Every run
drained, every call succeeded (943 of 943 on both builds at 8 agents; 3,724
of 3,724 and 3,729 of 3,729 at 16) and no worker died. At 16 agents 2,000 of
the acked receipts were queried: that is the harness's `--receipt-max`
sample cap (`"sampled": true`), not lost writes. Both 8-agent runs end with
3,698 concepts, all embedded.

Files: `conc-<build>-w<N>.json`.

At 8 agents the builds are close on every row. At 16 agents the medians are
close and 0.15.0's client-side recall p99 is higher in this run, 39.26 ms
against 24.56 ms, with a maximum of 327.62 ms; section 4.1 shows where that
comes from. Server apply p50 / p90 (`final_stats`, the most recent 256
applies) is 54 / 101 ms on 0.14.0 and 52 / 101 ms on 0.15.0.

## 4. The 16-agent runs

The figures in this section are server-side, from each serve's own ledger
([`ledger-summary.json`](ledger-summary.json), computed by
[`ledger_summary.py`](ledger_summary.py), which states the definitions). A
**slow call** is a recall or a write ack of 100 ms or more that ended more
than 5 s after the run's first call; the first recalls of a fresh serve are
cold and can exceed that. "N in M s" is N slow calls ending in M distinct
clock seconds.

The runs in time order: first pass (12:11 and 12:15), the two pairs of
section 4.2 (12:18 and 12:22), then four alternating runs with 0.15.0 first
(12:25 to 12:35). The 8-agent runs (12:06 and 12:09) have no slow call.

### 4.1 Six single runs

| First call | Build | Files | Recall p50 / p90 / p99 / max (ms) | Ack p99 / max (ms) | Apply lag p50 / p90 / p99 (ms) | Slow calls |
|---|---|---|---|---|---|---:|
| 12:11:49 | 0.14.0 | `conc-0140-w16` | 4.87 / 11.32 / 20.82 / 28.53 | 16.06 / 27.44 | 51.66 / 105.68 / 175.41 | 0 |
| 12:15:06 | 0.15.0 | `conc-0150-w16` | 4.78 / 11.18 / 25.28 / 294.67 | 18.76 / 382.08 | 51.14 / 100.23 / 191.73 | 10 in 1 s |
| 12:25:29 | 0.15.0 | `conc3-r1-0150-w16` | 4.87 / 11.30 / 23.98 / 38.11 | 20.54 / 51.75 | 49.71 / 105.40 / 184.28 | 0 |
| 12:28:40 | 0.14.0 | `conc3-r2-0140-w16` | 4.80 / 10.67 / 21.94 / 45.56 | 17.15 / 42.98 | 50.71 / 104.99 / 178.49 | 0 |
| 12:31:51 | 0.15.0 | `conc3-r3-0150-w16` | 4.85 / 11.36 / 21.71 / 107.24 | 17.57 / 108.59 | 50.07 / 100.12 / 185.91 | 2 in 1 s |
| 12:35:08 | 0.14.0 | `conc3-r4-0140-w16` | 4.80 / 10.78 / 20.22 / 42.23 | 17.29 / 31.19 | 51.41 / 102.82 / 186.63 | 0 |

- Recall p50 is between 4.78 and 4.87 ms in all six runs, and apply lag p50
  between 49.71 and 51.66 ms.
- **The first-pass 0.15.0 run has one stalled second.** All 10 of its slow
  calls ended in the second at 12:16:09, about 63 s into the load. In
  ten-second buckets from the first call (`buckets_10s`):

  | From (s) | Load calls | Recall p50 (ms) | Recall max (ms) | Ack max (ms) | Apply lag max (ms) |
  |---:|---:|---:|---:|---:|---:|
  | 50 | 311 | 4.41 | 16.36 | 14.49 | 129.59 |
  | 60 | 306 | 4.60 | 294.67 | 382.08 | 485.13 |
  | 70 | 293 | 4.82 | 20.10 | 15.16 | 167.21 |

  Recalls and write acks are slow in the same bucket. In every other bucket
  after the first, no recall took more than 29.32 ms and no ack more than
  24.76 ms.
- The other slow seconds of these runs hold 2 calls (0.15.0, 12:32:16) and
  none at all on 0.14.0. Over the three single runs a side that is 12 slow
  calls on 0.15.0 and 0 on 0.14.0.
- The one machine reading taken beside the alternating runs is the 1-minute
  load average printed at the start of each: 4.96, 3.16, 3.15 and 1.60 in
  run order (from the run log, not in a file here), where those runs had 0,
  0, 2 and 0 slow calls. Nothing was sampled during a run.
- No call failed and no write failed in any of the six.

### 4.2 Both builds at the same time

**Method:** one serve of each build, each on its own fresh copy and port,
each under its own `mcp_concurrent.py` with the arguments above, started
together 15 s after both serves answered: the first calls are at 12:18:49
on both sides of pair 1 and at 12:22:17 on both sides of pair 2
(`first_call_utc`). Whatever else the machine does reaches both at once.
Two serves were under load at once, with the live writer on the same GPU,
so apply lag is about double that of a single run (p50 96 to 98 ms against
about 50 ms). Only the two sides of a pair compare. Pair 2 swaps the start
order and the ports.

Files: `pair<N>-<build>-w16.json`, and the `pairs` block of
`ledger-summary.json`, which compares the two sides second by second over
the 115 s after both have warmed up.

| | Pair 1: 0.14.0 | Pair 1: 0.15.0 | Pair 2: 0.14.0 | Pair 2: 0.15.0 |
|---|---:|---:|---:|---:|
| Started | first | second | second | first |
| Recall p50 / p99 (ms) | 4.50 / 22.95 | 4.46 / 24.04 | 4.41 / 25.25 | 4.39 / 29.93 |
| Ack p99 / max (ms) | 19.77 / 66.29 | 22.01 / 29.20 | 24.17 / 98.55 | 27.32 / 67.33 |
| Apply lag p50 / p90 / p99 (ms) | 96.47 / 267.09 / 580.55 | 96.30 / 264.27 / 556.94 | 98.11 / 284.22 / 584.35 | 96.96 / 288.62 / 620.54 |
| Slow calls | 0 | 0 | 1 in 1 s | 0 |
| Seconds whose slowest call is 50 ms or more | 2 | 2 | 3 | 5 |
| ... of them the same second on both sides | 2 | 2 | 2 | 2 |
| Seconds in which this side's slowest call is the slower by over 5 ms | 16 | 27 | 15 | 30 |

- **Neither build stalls in either pair.** The one slow call is a 0.14.0
  recall of 112.08 ms in pair 2. No second on either side of either pair
  has a call of 250 ms or more.
- **Second by second, the slowest call leans slightly towards 0.15.0.** The
  median difference of the per-second slowest call, 0.15.0 minus 0.14.0,
  is +0.86 ms in pair 1 and +1.07 ms in pair 2, and 0.15.0's is the slower
  by over 5 ms in more of the 115 seconds in both pairs (27 against 16, 30
  against 15).
- **So do the upper percentiles.** 0.15.0 has the higher recall p99 in both
  pairs (by 1.09 and 4.68 ms) and the higher ack p99 (by 2.24 and 3.15 ms).
  The medians are within 0.04 ms.
- Over all five 16-agent runs a side, recall p99 ranges from 20.22 to 25.25
  ms on 0.14.0 and from 21.71 to 29.93 ms on 0.15.0. The ranges overlap,
  and the lowest 0.15.0 run (21.71 ms) is below three of the 0.14.0 runs.

### 4.3 The code a slow call runs through

In the 0.15.0 stall, recalls and write acks are slow in the same ten-second
bucket (section 4.1). Some of the code they share is unchanged between the
two tags: the ledger writer, SQLite persistence, the memory write and gate
paths and the write tool handler. This prints nothing:

```sh
git diff --numstat v0.14.0..v0.15.0 -- src/ledger.rs \
  src/store/sqlite/persistence.rs src/memory/writes.rs src/memory/gate.rs \
  src/mcp/server/tools/write.rs
```

The candle embedder's diff (`src/embed/candle.rs`, 21 lines added) is all
under `#[cfg(test)]`; `Cargo.toml` changes the version and adds `proptest`
as a dev-dependency, and `Cargo.lock` the version and proptest's packages.

Unlike the previous gate, the rest of the path did change. Leaving out
files under `tests/` and `*_tests.rs`, but counting test modules inside a
source file, `git diff --numstat v0.14.0..v0.15.0` reports 5,963 lines added
and 2,097 removed over the write queue (`src/writeq.rs`, `src/writeq`), the
flush loop (`src/store/flush.rs`, 1,010 and 129), store errors
(`src/store/error.rs`), the session builder, leases and shutdown
(`src/memory`), the load check (`src/store/load.rs`,
`src/graph/graph/invariants.rs`), recall (`src/recall`,
`src/graph/hybrid.rs`), the daemon (`src/daemon`), canonization
(`src/canon`), canonical keys (`src/graph/canonical.rs`) and the MCP serve
and server (`src/mcp`). These are the changes listed under "What 0.15.0
changes"; `src/recall/candidates.rs` (329 and 59) and `src/recall/dispatch.rs`
(207 and 108) are the largest recall changes, and `src/daemon/hotlist.rs`
(480 and 30) and `src/canon/policy.rs` (469 and 83) the largest outside the
write path. None of these was traced against the stall or the p99 lean.

### 4.4 What these runs do not show

- **The cause of the stall in `conc-0150-w16`.** One second, 10 calls, in
  one of five 0.15.0 runs. There was no 0.14.0 run under load at that
  moment to compare against, and nothing was sampled on the machine.
- **Whether the p99 lean belongs to 0.15.0.** It is a few milliseconds,
  seen in two pairs, with ranges that overlap across the ten runs. The
  changed recall and write code (section 4.3) could account for it or not;
  the runs were not randomised and were not repeated.
- **How this compares with the previous gate.** The same 0.14.0 binary,
  harness and arguments gave 222 slow calls over six 16-agent runs in the
  previous gate (its section 4.5) and 1 over five here; the two gates ran
  on different days and slightly different snapshots. The machine's state
  was not recorded on either day beyond the load averages.

## 5. Session load and the canonization dry run

**Method:** hyperfine, both builds against one more copy of the snapshot
(`store.db`), after the replay. Both commands only read it; the dry-run
reports confirm their source was unchanged.

- `lambo stats --config lambo.toml --session lambo`, 30 runs per build.
  This loads the session the way a writer attach does (`load_session_async`,
  which validates the graph's invariants, `src/cli/mod.rs`), so on 0.15.0 it
  includes the #325 two-pass cycle check.
- `lambo canon dry-run --store store.db --session lambo --at
  2026-11-22T11:32:30Z --json`, 8 runs per build. The clock is pinned one
  second after the snapshot, so both builds survey the same state. Each run
  surveys both promotion policies.

Files: [`timing-stats-load.json`](timing-stats-load.json),
[`timing-canon-dryrun.json`](timing-canon-dryrun.json),
[`stats-session-0140.txt`](stats-session-0140.txt) and
[`stats-session-0150.txt`](stats-session-0150.txt),
[`canon-dryrun-0140.json`](canon-dryrun-0140.json) and
[`canon-dryrun-0150.json`](canon-dryrun-0150.json).

| Command | Build | Runs | Mean / median (ms) | Min / max (ms) | Peak RSS, mean (MiB) |
|---|---|---:|---|---|---:|
| `stats --session lambo` | 0.14.0 | 30 | 149.3 / 148.8 | 141.4 / 159.3 | 99.3 |
| `stats --session lambo` | 0.15.0 | 30 | 145.2 / 144.2 | 140.2 / 165.4 | 99.9 |
| `canon dry-run` | 0.14.0 | 8 | 2,123.2 / 2,123.8 | 2,113.3 / 2,133.1 | 92.6 |
| `canon dry-run` | 0.15.0 | 8 | 2,141.9 / 2,140.9 | 2,132.5 / 2,154.5 | 94.4 |

Every run exited 0. The two `stats` outputs are identical: 3,820 nodes,
7,299 edges, 2,743 concepts, all embedded, 5 Canonical.

- **Session load:** 0.15.0's mean is 4.1 ms lower, with the second cycle
  pass included. The standard deviations are 4.3 and 4.8 ms.
- **Dry run:** 0.15.0's mean is 18.7 ms (0.9%) higher, and every 0.15.0 run
  is slower than the 0.14.0 median. Peak RSS is 1.8 MiB higher.

**The dry-run reports.** In both reports every invariant held, the source
store's hash is the same before and after, and both see 2,743 concepts,
7,299 edges and 1,077 interactions. A field-by-field diff of the two
reports differs only inside the Solo policy's block:

- **Swarm:** identical. 30 cycles to a fixed point, 5 Canonical, the same
  status counts at the end (444 Candidate, 15 Venerable).
- **Solo:** 33 cycles to a fixed point on both, 5 Canonical and 15
  Venerable on both. 149 concepts reach Candidate on 0.14.0 and 146 on
  0.15.0 (`reached.Candidate.total`; Constraint 13 against 11, Logic 57
  against 56), so 0.15.0 ends with 590 Candidates against 593. The first
  evaluation's `stage1/solo_score` near-misses rise from 242 to 245, and
  the first cycle moves 50 concepts on both builds (45 non-Resource and 5
  Resource on 0.14.0, 44 and 6 on 0.15.0).

Fewer Solo Candidates is the direction the CHANGELOG's #300 entry
describes for Solo ("existing Solo Candidates and Venerables can lose
score"), and Swarm is unchanged, as that entry says. The reports list
counts, not the concepts that moved, so the gate did not check which three
concepts these are or trace them to #300.

## 6. The re-pin

The rig was re-pinned to 0.15.0 at 12:49 the same day (18:19 local;
DOGFOOD-SETUP §2a). None of this section is in a file here; it is from the
rig's ledger, log, backup and MCP answers.

- The pre-re-pin backup holds 7,735 concepts.
- `provision` by 0.15.0 on the live store re-keyed 0 concepts and changed
  no schema.
- The 0.14.0 writer exited within a second of the stop signal and released
  its leases; all 8 session leases were held by the new process within
  about 6 s. The previous re-pin took about 45 s because the old writer did
  not finish shutting down (previous gate, section 5); this one did not
  repeat that.
- The first ledger heartbeat reported git sha 8f5fdd0, not degraded, 0
  dead-lettered.
- A live recall answered.
- The writer's log has no WARN or ERROR line since the restart.

## Not covered

- Replay drain timing and the #295 replay order. The snapshot had no owed
  intent (all 10 applied), so no serve had a backlog to drain:
  `write_queue_replay_owed` and `write_queue_replayed` are 0 in every
  `conc*.json` and `pair*.json`.
- The #295 in-session hold and `pending_replay`, the #294 replay refusals,
  and the reworked counters, under a real embedder outage. No harness
  stops the embedder; `write_queue_outstanding` and `write_queue_failed` end
  at 0 and `write_queue_replay_blocked` at `null` in every run.
- The #293 lease fencing. No takeover was staged; the only lease handover
  seen is the live re-pin (section 6).
- The #301 flush and store-error paths. No store error occurred
  (`dead_lettered` 0 and `degraded` false in every `final_stats`).
- The #301 key change. The re-key changed no key in this store, and no
  harness write was checked for a `''s` token.
- A session holding a mixed `parent_of`/citation cycle (#325). Every
  session of the snapshot loads on 0.14.0, so none holds one; section 5
  times the second pass on a session without one.
- The recall changes (#328, #288, #299, #285) as such. The 20 queries'
  ranked hits are identical on both builds, but whether any of them meets a
  zero-score tie, an exact durable-name match or a dependency phrase was
  not checked, and no harness reads the `names` field.
- The daemon changes (#297, #298, #302). No harness reads conflict
  warnings, the hot list or replacement candidates; `probe-top8.json`
  counts warning lines only.
- Solo canonization in a live serve (#300). Only the dry run covers it, and
  the rig runs Swarm.
- Store-wide session discovery and its listing in `lambo serve-web` (#276,
  PR #287).
- `lambo re-embed` (#296, #322), `--auth-token` parsing (#315), the
  Elasticsearch recall tier (#319) and `lambo_inspect` with durable names
  (#285).
- Ledger heartbeats under load. The gate serves ran with `--ledger` and
  without `--ledger-heartbeat`, as in the previous gate.
- The cause of the stalled second in section 4.1 and of the p99 lean in
  section 4.2.
- The CUDA rig. This gate ran on the Metal rig alone.
