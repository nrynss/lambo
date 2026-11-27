# 0.15.0 and 0.16.0 on the CUDA rig: the re-pin gate

The RTX 4070 SUPER dogfood rig's run of the re-pin gate for the step from
0.15.0 to 0.16.0. The last gate on this rig is
[0.12.0 and 0.15.0](../0.15.0-vs-0.12.0-cuda/README.md) (2026-11-22). This one
follows the method of the Metal gate
[0.16.0 and 0.15.0](../0.16.0-vs-0.15.0-metal/README.md): the same harnesses
and arguments, the same-build recall control, the alternating 16-agent runs
and the paired 16-agent runs, and the same helper scripts retargeted to the
two builds.

- **When:** 2026-11-27. Times below are UTC.
- **Builds:** OLD is 0.15.0 (binary `lambo-0.15.0`, `lambo 0.15.0`, merge
  commit 8f5fdd07). The candidate is 0.16.0 (tag `v0.16.0`, merge commit
  28efff5c), built from source with the same features. There is no CUDA
  release asset. The build command and its environment fixes are in section 7.
  The live 0.15.0 writer stayed up on the same GPU for every gate run and was
  replaced only by the re-pin (section 6).
- **Machine:** RTX 4070 SUPER, 12 GB, CUDA 13.5 (nvcc V13.5.37), the same rig
  as the previous CUDA gate. Both gate builds embed with candle BGE-M3 on CUDA
  (1024-d). This is a different rig from the Metal gate: M3 Pro, Metal, a
  2,743-concept largest session with a `general` layer, against this rig's
  11,347-concept largest session and no attached layer. No figure from one rig
  is combined with a figure from the other.
- **Store:** a SQLite `.backup` of the serving store taken at 02:53 while the
  writer was live: 11,372 concepts across 5 sessions, all embedded. The gate
  ran against the largest session, `lambo-dev` (11,347 concepts, 27,440 edges,
  3,929 interactions), with no layer attached, as in the live config. The
  snapshot's newest interaction is at 2026-11-26T15:21:20Z; its newest dated
  row is an `event_time` at 2026-11-26T15:30:00Z. Every run used its own fresh
  copy, provisioned by the build that served it. The counts are from the
  snapshot, not in a file here; the replay file's `store.base` holds the
  post-truncation counts.
- **Pending writes:** the snapshot's 28 `write_intents` rows were all applied;
  none had `consumed_at IS NULL` (from the snapshot, not in a file here). The
  replay removed the 28 with the truncated history (`meta.truncation`), and
  every concurrent run ends with `write_queue_replay_owed` and
  `write_queue_replayed` at 0 (`final_stats`).
- **Provisioning:** both builds provision the snapshot idempotently (`sqlite
  schema provisioned (init_schema, idempotent)`) and leave
  `store_meta.canonical_key_rule` at `301-empty-stems-dropped`; no key was
  re-keyed. 0.16.0's provision on the live store did the same at the re-pin
  (section 6).
- **Promotion policy:** the live `lambo.toml` has no `promotion_policy`, and the
  unit's environment sets none; the live heartbeat after the re-pin reports
  `promotion_policy = Swarm`. The gate serves ran Swarm too. Checked on the
  rig; not in a file here.
- **Machine load:** the machine carried a 1-minute load average of about 3–4
  between gate phases, almost all of it from the desktop's own Electron
  processes (opencode, ZCode); the 15-minute average reached about 9.8 over
  the 16-agent phase, against 0.78–1.51 logged between runs at the 0.15.0
  gate. CPU pressure after the gate was at most 0.18 in the 10-second average
  (`some avg10`). Absolute latencies are therefore higher than the 0.15.0
  gate's and are not comparable with it; only the two builds of this gate
  compare (section 4.3).

These files hold no query text and no concept text from the store, and no
absolute paths. The recall probe produced no warning lines at all on this rig,
so `probe-recall.txt` carries counts, id prefixes, scores, concept types and
session names only.

- The harness files (`sequential-ab-*.json`, `conc*.json`, `pair*.json`) and
  `probe-recall.txt` are as captured and hold aggregates, except that
  `probe-recall.txt` lists the hits of queries by node id prefix, score,
  concept type and session.
- `replay-0150-0160.json` has one row per replayed call: its kind, outcome
  counts and timings, and for the 300 real calls the id of the agent that made
  the original write and the interaction's id, both as stored. Its three path
  fields (the two binaries and the snapshot) are reduced to file names; nothing
  else in it is edited.
- `timing-stats-load.json`, `timing-stats-load-r2.json`,
  `timing-canon-dryrun.json` and `timing-canon-dryrun-r2.json` are hyperfine
  2.0.0 exports (raw `measurements` and per-command `summary`). Their `command`
  fields are reduced to file names; nothing else is edited.
  `stats-session-0150.txt` and `stats-session-0160.txt` are the output of the
  timed `lambo stats` command, one per build.
- `canon-dryrun-0150.json` and `canon-dryrun-0160.json` are the two
  `lambo canon dry-run --json` reports, with `source.path` reduced to its file
  name. They hold counts and full concept ids, and no text.
- `ledger-summary.json`, `ab-ledger-top8.json` and `probe-top8.json` are
  computed by the scripts beside them, from each serve's own call ledger and
  from the probe's saved recall bodies. Those inputs hold memory text and stay
  on the rig. The scripts are the Metal gate's, with the build tags in their
  docstrings changed.

## What 0.16.0 changes

From the `## 0.16.0` section of `CHANGELOG.md`. Inside a serving writer:

- **Startup replay (0.16.0).** Startup replay isolates a rejected write,
  reports it `failed` after the store confirms its outcome, and keeps later
  intents available for a clean restart. It refuses new writes during startup
  replay and while outcomes await confirmation (#324).
- **Serve preflight (0.16.0).** A bind beyond loopback with no credential, and
  a misused `--ledger-heartbeat` (without `--ledger`, or `0`), are refused
  before any backend is built, with exit 2 (#316).
- **Close and dead letters (0.16.0).** A close no longer answers
  `intent_durable` for a write whose intent a dead letter dropped; it answers
  `close_unflushed` with reason `intent_dead_lettered` (#345).
- **Lookup-timeout streak (0.16.0).** A lookup timeout that follows a
  successful write no longer counts toward the run of slow lookups; the streak
  resets inside the write's commit (#346).
- **Leases and flush (0.16.0).** An HTTP erase conflict now explains that the
  live lease can still belong to the same serve after it detaches the session
  (#314); a stale background flush wakes serve as soon as it fences the writer
  (#303); a delayed lease-loss detach can only close the HTTP attachment that
  lost its lease (#312); and closing a session reports an error after a
  background flush task panic and still attempts to flush surviving writes
  (#318).
- **Classification (0.16.0).** Dependency-question classification and subject
  matching use the same Unicode normalization for marker boundaries (#306).

The rest is outside the serve loop: the Candle, MCP, SQLite and TLS dependency
updates (#352), PostgreSQL canonical keys beyond the B-tree text size limit
(#323), Elasticsearch mirror repair (#321), `lambo serve-web` no longer
loading the embedder at startup (#247), MCP `agent_id` control-character
errors (#338), the formal-model reorganization, the repository cleanup (the
frozen v0.1 spec and `dev-diary/` are removed), the llama.cpp embedding script
fixes, the `candle` weight-cache and `HF_HOME` fix, and the new guides.

None of the upgrade notes apply to this rig: the store is SQLite (the
PostgreSQL re-index does not run), `canonization_min_peer_count` is not set,
the embedder is `candle` (not `bedrock`/`titan`), and the build used rustc
1.100.0. No re-embed was needed; the embedding contract is unchanged (candle,
BGE-M3, dim 1024) and the pre- and post-re-pin `lambo_stats` both report
11,347/11,347 embedded.

## Summary

Latency rows are client-side from the harness files unless marked (server),
which are from `ledger-summary.json` and leave out each run's first 5 s
(definition in section 4).

| Test | 0.15.0 | 0.16.0 | Verdict |
|---|---:|---:|---|
| Replay of 300 real writes and 30 variants | 330 applied | 330 applied | PASS; 0 of 330 calls differ (section 1) |
| Top-8 recall on the 20 A/B queries | | | identical ids in the same order on all 200 pairs; `recall_topk_jaccard` mean 1.000 (section 2) |
| Same 20 queries, two OLD serves and one 0.16.0 | | | mean Jaccard 1.000 between builds and 1.000 between the two 0.15.0 serves (section 2) |
| Sequential recall p50 / p99 | 18.97 / 96.83 ms | 19.27 / 101.76 ms | 0.30 ms higher at p50 on 0.16.0, one run (section 2) |
| Sequential derive ack p50 / p90 | 17.23 / 102.47 ms | 13.06 / 110.00 ms | 0.16.0 lower at p50 (section 2) |
| Sequential derive→applied p50 / p90 | 84.41 / 138.53 ms | 83.76 / 140.83 ms | level (section 2) |
| 8 agents: writes failed | 0 / 531 | 0 / 532 | none failed on either |
| 8 agents: recall p50 / p99 (server) | 24.86 / 186.04 ms | 25.46 / 280.82 ms | p50 level; p99 higher on 0.16.0 in this one pair of runs (section 3) |
| 16 agents, single runs: writes failed | 0 | 0 | none failed on either |
| 16 agents, single runs: recall p50 (server) | 107.90–132.09 ms | 113.64–138.74 ms | overlapping ranges (section 4.1) |
| 16 agents, single runs: ack p50 (server) | 98.28–119.68 ms | 88.38–98.65 ms | lower on 0.16.0 in all three blocks (section 4.1) |
| 16 agents, single runs: apply lag p50 (server) | 180.72–215.23 ms | 205.65–245.52 ms | higher on 0.16.0 in two of three blocks (section 4.1) |
| 16 agents, single runs: slow calls (server) | 1,465–1,604 | 1,360–1,499 | lower on 0.16.0 in all three blocks (section 4.1) |
| 16 agents, two pairs: recall p99 (server) | 286.64 / 553.11 ms | 375.14 / 588.11 ms | ratios 1.31 / 1.06; per-second median follows startup order (section 4.2) |
| 16 agents, two pairs: slow calls (server) | 1,652 / 1,626 | 1,498 / 1,486 | lower on 0.16.0 in both pairs (section 4.2) |
| Writes failed, all 12 concurrent runs | 0 | 0 | none failed on either |
| Session load, `lambo stats --session lambo-dev`, 30 runs, mean | 1,115 / 1,112 ms | 1,087 / 1,079 ms | 2.4–3.0% lower on 0.16.0 (section 5) |
| Canonization dry run, 8 runs, mean | 31.38 / 31.49 s | 31.47 / 31.40 s | level, +0.3% and −0.3% (section 5) |
| Canonization dry-run reports | | | identical outside `source`; all invariants held (section 5) |

**Verdict: the gate passes and the rig was re-pinned to 0.16.0 the same day
(section 6).** Replay is identical (0 of 330 calls differ). The ranked top 8
is identical on all 200 A/B pairs and in the same-build probe, where both
between-build and same-build Jaccard are 1.000. No write failed in any of the
12 concurrent or paired runs, and no harness reported an error or a dead
worker. Medians are level or mixed: session load is 2.4–3.0% faster on 0.16.0
in both release orders, the canonization dry run is level, sequential recall
p50 differs by 0.30 ms, and derive acknowledgement p50 is lower on 0.16.0 in
every concurrent run. The one consistent lean the other way is apply-lag p50,
higher on 0.16.0 in four of five comparisons and lower in one, inside the
run-to-run spread of both builds (180.72–250.18 ms across this gate's runs).
In the two simultaneous pairs, the per-second slowest-call median follows the
startup order — the side started second is slower in both, whichever build it
is — while recall p99 is higher on 0.16.0 in both pairs (ratios 1.31 and 1.06)
and slow-call counts are lower on 0.16.0 in both. The largest p99 difference is
31%, well below the 2× no-re-pin threshold, and the six single runs' recall p99
ranges overlap (327.90–467.98 ms on 0.15.0, 302.78–550.44 ms on 0.16.0).

## 1. Replay parity

**Method:** `replay_parity.py`, the last 300 real write calls of session
`lambo-dev` (127 derive, 173 record_action) plus 30 reworded variants, with
`--canon-eval-secs 100000000 --canon-wait 0`. The built-in reason catalogue
needed no new entries. Because the snapshot is from this rig and not the Metal
rig's, these are different calls from the Metal gate's replay. The truncation
removed 1,350 edges, 541 concepts, 307 interactions, the 28 write intents and
the 5 lease rows (`meta.truncation`).

File: [`replay-0150-0160.json`](replay-0150-0160.json), verdict in the harness
log (`parity.txt` stays on the rig with the work tree).

**0 of 330 calls differ.** Both sides applied 330 of 330, created 544
concepts, semantically merged 27, embedded all 544, and wrote 390 new edges.
Both ended on stores with 11,375 concepts, all embedded, 27,522 edges with the
same count per edge type, 3,976 interactions, and the same canonization status
counts (1,289 Candidate, 4 Canonical, 10,072 None, 10 Venerable). No created
concept and no semantic edge exists on one side only. 45 calls carry
reconstruction warnings.

## 2. Sequential latency A/B

**Method:** `mcp_ab.py ab`, two HTTP serves side by side, 10 rounds of 20
queries interleaved per query, then 30 derives per side each followed by a
receipt wait. 0.15.0 is listed first and was started first. The script waited
90 s after both serves answered before starting the harness. Following the
newer Metal gate, both serves ran with `--rate-limit-rps 0`; neither side
retried a rate-limited request (`rate_limited_retries` 0/0), and neither
reported an error.

File: [`sequential-ab-0150-0160.json`](sequential-ab-0150-0160.json).

| Client measurement (ms) | 0.15.0 p50 / p90 / p99 | 0.16.0 p50 / p90 / p99 |
|---|---:|---:|
| Recall | 18.97 / 28.64 / 96.83 | 19.27 / 29.37 / 101.76 |
| Stats | 1.42 / 1.84 / 2.23 | 1.43 / 1.80 / 2.15 |
| Derive acknowledgement | 17.23 / 102.47 / 275.81 | 13.06 / 110.00 / 126.64 |
| Derive to applied receipt | 84.41 / 138.53 / 299.62 | 83.76 / 140.83 / 171.00 |

`recall_topk_jaccard` has mean 1.000 over the 20 queries: every query returned
the same set on both builds.

### What the Jaccard is

[`ab-ledger-top8.json`](ab-ledger-top8.json), computed by
[`ab_ledger_top8.py`](ab_ledger_top8.py) from the two serves' ledgers, pairs
the two sides' 200 recalls. The first eight hits are the same ids in the same
order on all 200 pairs, with a score difference of 0.0 everywhere. No ninth
hit appears on either side (`first_round_set_overlap_mean` 1.000,
`first_round_queries_with_overlap_below_1` empty).

### The same-build control

**Method:** [`probe_recall.py`](probe_recall.py) asks the same 20 queries, with
the same warm-up and `top_k = 8`, of three serves on fresh copies: two
instances of 0.15.0 (`0150a`, `0150b`) and one of 0.16.0, all answering at
03:49. Three rounds; the script waited 60 s after the last serve answered. It
ran after the 16-agent runs.

Files: [`probe-recall.txt`](probe-recall.txt) and
[`probe-top8.json`](probe-top8.json), computed from its saved bodies by
[`probe_top8.py`](probe_top8.py).

- The first eight hits are the same on all three serves, same ids in the same
  order with score difference 0.0, for every query in every round.
- Mean Jaccard is 1.000 between 0.15.0 and 0.16.0 and 1.000 between the two
  0.15.0 serves. No response has a ninth hit and no response carries a warning
  line.

## 3. Concurrent agents

**Method:** `mcp_concurrent.py` for 120 s, default mix, seed 1; 8 workers with
1.0 s think time. One serve at a time, each on a fresh copy, default rate
limit, 0.15.0 first. Both runs drained, every call succeeded and no worker
died.

Files: `conc-0150-w8.json`, `conc-0160-w8.json`.

| Run | Writes acked | Failed | Recall p50 / p99 (server) | Ack p50 (server) | Apply lag p50 / p90 (server) | Slow calls | Recall p50 / p99 (client) |
|---|---:|---:|---|---|---|---:|---|
| `conc-0150-w8` | 531 | 0 | 24.86 / 186.04 ms | 1.30 ms | 76.60 / 165.84 ms | 89 | 47.67 / 208.23 ms |
| `conc-0160-w8` | 532 | 0 | 25.46 / 280.82 ms | 0.36 ms | 75.66 / 179.73 ms | 91 | 42.99 / 284.95 ms |

The server-side figures, which exclude each run's first 5 s, are the
comparable ones here. On this rig, with the machine loaded as described in the
intro and section 4.3, the 100 ms slow-call threshold sits near the store's
ordinary p90; both sides have tens of them.

## 4. The 16-agent runs

The figures in this section are server-side, from each serve's own ledger
([`ledger-summary.json`](ledger-summary.json), computed by
[`ledger_summary.py`](ledger_summary.py), which states the definitions). A
**slow call** is a recall or a write ack of 100 ms or more that ended more than
5 s after the run's first call. "N in M s" is N slow calls ending in M distinct
clock seconds.

### 4.1 Six single runs

| Run | Build | File | Recall p50 / p90 / p99 / max (ms) | Ack p99 / max (ms) | Apply lag p50 / p90 / p99 (ms) | Slow calls |
|---|---|---|---|---|---:|
| 03:21 | 0.15.0 | `conc-b1-0150-w16` | 107.90 / 194.77 / 327.90 / 460.06 | 323.06 / 552.38 | 180.72 / 345.82 / 619.41 | 1465 in 117 s |
| 03:25 | 0.16.0 | `conc-b1-0160-w16` | 113.64 / 196.64 / 302.78 / 425.74 | 287.76 / 415.76 | 205.65 / 405.69 / 618.99 | 1407 in 116 s |
| 03:28 | 0.16.0 | `conc-b2-0160-w16` | 114.68 / 246.30 / 550.44 / 1000.21 | 377.55 / 993.41 | 211.73 / 494.47 / 964.55 | 1360 in 117 s |
| 03:31 | 0.15.0 | `conc-b2-0150-w16` | 132.09 / 295.28 / 467.98 / 788.99 | 503.90 / 794.30 | 215.23 / 488.88 / 882.87 | 1604 in 116 s |
| 03:35 | 0.15.0 | `conc-b3-0150-w16` | 111.52 / 208.75 / 381.19 / 665.44 | 392.76 / 880.78 | 192.88 / 406.81 / 782.74 | 1504 in 117 s |
| 03:38 | 0.16.0 | `conc-b3-0160-w16` | 138.74 / 270.68 / 522.10 / 707.48 | 412.28 / 694.02 | 245.52 / 537.54 / 950.24 | 1499 in 116 s |

- Recall p50 is between 107.90 and 138.74 ms across the six runs, and apply
  lag p50 between 180.72 and 245.52 ms.
- The three blocks ran 0.15.0 then 0.16.0, then 0.16.0 then 0.15.0, then
  0.15.0 then 0.16.0. Machine load is discussed in section 4.3.
- No call failed and no write failed in any of the six. Every run's
  `final_stats` has `write_queue_failed` 0, `dead_lettered` 0,
  `degraded` false, and `write_queue_replay_owed` and `write_queue_replayed`
  both 0.

### 4.2 Both builds at the same time

**Method:** one serve of each build, each on its own fresh copy and port, each
under its own `mcp_concurrent.py` with the 16-agent arguments (seed 1, 120 s,
`--receipt-max 5000`), started together 15 s after both serves answered.
Whatever else the machine does reaches both at once. Two serves were under
load at once with the live writer on the same GPU. Only the two sides of a
pair compare. Pair 1 started 0.15.0 first; pair 2 started 0.16.0 first. Each
pair's first calls are at the same second on both sides.

Files: `pair1-0150-w16.json`, `pair1-0160-w16.json`, `pair2-0150-w16.json`,
`pair2-0160-w16.json`, and the `pairs` block of `ledger-summary.json`, which
compares the two sides second by second over the 115 s after both have warmed
up.

| | Pair 1: 0.15.0 | Pair 1: 0.16.0 | Pair 2: 0.15.0 | Pair 2: 0.16.0 |
|---|---:|---:|---:|---:|
| Started | first | second | second | first |
| Recall p50 / p99 (ms) | 129.85 / 286.64 | 135.72 / 375.14 | 137.85 / 553.11 | 145.59 / 588.11 |
| Ack p99 / max (ms) | 330.21 / 498.19 | 336.48 / 535.32 | 569.34 / 961.88 | 435.65 / 890.54 |
| Apply lag p50 / p90 (ms) | 210.01 / 398.69 | 242.07 / 486.64 | 235.13 / 648.56 | 250.18 / 562.74 |
| Writes applied | 1735 | 1711 | 1574 | 1655 |
| Slow calls | 1652 in 116 s | 1498 in 116 s | 1626 in 116 s | 1486 in 117 s |
| Seconds with slowest call >= 250 ms | 45 | 74 | 73 | 79 |
| ... >= 500 ms | 0 | 1 | 29 | 21 |
| Seconds in which this side's slowest call is the slower by over 5 ms | 36 | 73 | 50 | 60 |
| Median per-second slowest call, 0.16.0 minus 0.15.0 | | +30.54 ms | −10.22 ms | |
| Seconds where both sides have a slow call | 116 | 116 | 116 | 117 |

- **The pair tail is mixed.** Recall p99 is higher on 0.16.0 in both pairs
  (ratios 1.31 and 1.06), while slow-call counts are lower on 0.16.0 in both.
  The per-second slowest-call median follows the startup order: the side
  started second is slower in both pairs (0.16.0 by 30.54 ms in pair 1,
  0.15.0 by 10.22 ms in pair 2), and the seconds-slower counts are mixed the
  same way (pair 1: 36 / 73; pair 2: 50 / 60).
- No pair is near the 2× no-re-pin threshold. The highest p99 ratio is 1.31,
  and the six single runs' recall p99 ranges overlap.
- Apply lag p50 is 210–250 ms in these pairs, against 181–246 ms in the
  single runs; nothing in these runs establishes why.
- No call failed and no write failed in any of the four runs, and every run
  drained with the same zeroed queue counters as section 4.1.

### 4.3 Comparing levels with the 0.15.0 gate

The 0.15.0 gate ran with a 1-minute load average of 1.04 before its timings
and logged 0.78–1.51 between its 16-agent runs. This gate ran under a much
heavier machine: a 1-minute load average of about 3–4 between phases and a
15-minute average near 9.8 over the 16-agent phase, with the desktop's own
Electron processes as the persistent contributors. Its absolute levels are
correspondingly higher: server recall p50 is 108–146 ms here against 37–44 ms
then, apply lag p50 181–250 ms against 97–105 ms, and the `stats` session load
1.09–1.12 s against 0.83 s.

That difference is environmental and is not attributed to either build: both
builds were interleaved in every block and pair, and CPU pressure after the
gate had fallen to at most 0.18 in the 10-second average (`some avg10`). No
figure from this gate is compared with a figure from the 0.15.0 gate, and the
two rigs' figures are not compared either. What this gate supports is the
relative comparison: within it, the build deltas sit inside the run-to-run
spread, except for the order effect on the per-second median described in
section 4.2 and the ack/apply split described in the summary.

## 5. Session load and the canonization dry run

**Method:** hyperfine 2.0.0, both builds against one more copy of the
snapshot, with no serve attached, after all load runs had finished. Both
commands only read it; the dry-run reports confirm their source was unchanged
(the same sha256 before and after). The 1-minute load average was about 3
throughout the timings (the desktop's Electron processes), with the gate's own
load no longer active.

- `lambo stats --config lambo.toml --session lambo-dev`, 30 runs per build,
  2 warm-ups, then the same 30-run batch in reverse release order.
- `lambo canon dry-run --store store.db --session lambo-dev --at
  2026-11-26T15:30:01Z --json`, 8 runs per build in each release order. The
  clock is one second after the newest dated row in the snapshot, which is an
  `event_time`; an earlier first attempt at 15:21:21Z left one interaction and
  four edges out of the survey, the dry run reported its
  `clock_after_the_data` invariant as not held, and both builds reported the
  same truncated state. The reported runs use the corrected clock and all
  three invariants held.

Files: [`timing-stats-load.json`](timing-stats-load.json),
[`timing-stats-load-r2.json`](timing-stats-load-r2.json),
[`timing-canon-dryrun.json`](timing-canon-dryrun.json),
[`timing-canon-dryrun-r2.json`](timing-canon-dryrun-r2.json),
[`stats-session-0150.txt`](stats-session-0150.txt),
[`stats-session-0160.txt`](stats-session-0160.txt),
[`canon-dryrun-0150.json`](canon-dryrun-0150.json) and
[`canon-dryrun-0160.json`](canon-dryrun-0160.json).

| Command | Release order | 0.15.0 mean / median (ms) | 0.16.0 mean / median (ms) | Peak RSS, mean (MiB) |
|---|---|---:|---:|---:|
| `stats --session lambo-dev` | 0.15.0 first | 1115 / 1110 | 1087 / 1077 | 490 / 487 |
| `stats --session lambo-dev` | 0.16.0 first | 1112 / 1112 | 1079 / 1067 | 491 / 487 |
| `canon dry-run` | 0.15.0 first | 31,378 / 31,378 | 31,473 / 31,516 | 434 / 416 |
| `canon dry-run` | 0.16.0 first | 31,493 / 31,425 | 31,401 / 31,467 | 418 / 416 |

Every run exited 0. The two `stats` outputs are byte-identical: 15,276 nodes,
27,440 edges, 11,347 concepts, all embedded, and 5 Canonical concepts. Session
load is 2.4–3.0% faster on 0.16.0 in both orders and the dry run is level
(+0.3% and −0.3%, inside the run-to-run spread of both builds).

**The dry-run reports.** Both reports are identical outside `source`: every
invariant held, the source hash is the same before and after, and both see
11,347 concepts, 27,440 edges and 3,929 interactions with
`survival_raised` 0. Both policies reach the same fixed point on both builds:
Swarm in 30 cycles and Solo in 51. Swarm ends at 1,311 Candidate, 5 Canonical,
10,017 None and 14 Venerable, and Solo at 1,815 Candidate, 5 Canonical, 9,513
None and 14 Venerable. There is no policy difference to attribute to 0.16.0's
changes; the CHANGELOG's serve-loop entries do not touch canonization.

## 6. The re-pin

The rig was re-pinned to 0.16.0 at 04:18 (09:48 local; DOGFOOD-SETUP §2a).
None of this section is in a file here; it is from the rig's ledger, journal,
backup and MCP answers.

- The unit file was copied aside as `lambo-dogfood.service.bak-0.15.0`, and
  its `ExecStart` now names `lambo-0.16.0` directly with the same session,
  agent, port, ledger and heartbeat interval. `systemctl --user show` lists
  one `ExecStart` match. 0.15.0 remains installed as `lambo-0.15.0`; rollback
  is to stop the service, restore `.bak-0.15.0`, run daemon-reload and start
  again, restoring the pre-repin store backup only if the live store's
  identities had changed (they did not; see below).
- The old writer exited on the stop signal without waiting out the lease TTL,
  and released its leases. `stats --store` showed `no writer` on all five
  sessions before the swap, and no `write_intents` row had
  `consumed_at IS NULL`.
- The pre-repin backup is `backup-pre-0.16.0-20261127T041754Z.db` with a
  `.sha256` beside it
  (b4720d2ca3ed4e66ffa18360fc662cf91c265d8d464600fa5e19c65a10fd2e30);
  `integrity_check` on the copy printed `ok`.
- `provision` by 0.16.0 on the live store re-keyed 0 concepts and left the
  schema unchanged; `canonical_key_rule` stayed `301-empty-stems-dropped`.
  The installed binary's sha256 is
  a9c13eb3397063f77e3d2906b7eadbfee55a9b22e16e1db3d61a01d57d602912,
  the same as the built candidate.
- The new writer is pid 2218306 and stayed up with the same main pid over a
  65-second check. The `lambo-dev` lease is held by
  `http-shared-writer@cachyos-x8664#2218306`; the other four sessions have no
  writer, as before the swap (this unit pins `lambo-dev` only).
- The first ledger heartbeat after the start reports version 0.16.0 and git
  sha 28efff5. The journal since the restart has no WARN or ERROR line,
  including no embedder canary drift and no `missing_vectors` warning.
- A live `lambo_stats` names session `lambo-dev`, reports
  `embedded=11347/11347`, `dead_lettered = 0` and `degraded = false`; a live
  `lambo_recall` returns ranked hits with `top_legs.vector_cosine` 0.777 and
  no embedder warning.

## 7. Build notes

Built from tag `v0.16.0` (28efff5c) in a detached worktree, with the features
0.15.0 was built with:

```sh
NVCC_CCBIN=clang CUDARC_CUDA_VERSION=13040 LAMBO_GIT_SHA=28efff5 \
  cargo build --locked --release \
  --features store-sqlite,embed-candle-cuda,embed-bge
```

`CUDARC_CUDA_VERSION=13040` pins cudarc's bindings to CUDA 13.4 against the
rig's nvcc 13.5.37, following the rig's earlier ruling that 13.x minor
versions are ABI compatible; this is an environment fix, not a code change.
The build finished in 9 m 14 s. The binary was installed as `lambo-0.16.0`,
and `--version` reports `lambo 0.16.0`.

`embed-candle-cuda` is not compiled in CI, so this build is the only check
that the feature compiles at this tag on this toolkit. hyperfine 2.0.0 was
already installed on the rig from the 0.15.0 gate.

## Not covered

- The #324 startup replay isolation and the #295 replay order under a
  backlog. The snapshot had no owed intent (all 28 consumed), so no serve had
  one to replay: `write_queue_replay_owed` and `write_queue_replayed` are 0 in
  every concurrent and paired run.
- The #345 close and dead-letter accounting and the #346 lookup-timeout
  streak. No store error or lookup timeout was staged; `dead_lettered` is 0
  and `degraded` false in every `final_stats`.
- The #316 preflight refusals. No exposed bind and no misused
  `--ledger-heartbeat` were run.
- The #314 erase conflict, #312 delayed lease-loss detach and #303 stale-flush
  fence. No takeover or lease-loss path was staged; the only lease handover
  seen is the live re-pin (section 6).
- #318's close after a background flush panic. No flush panic was staged.
- The #306 Unicode normalization as such. The 20 queries' ranked hits are
  identical on both builds, but whether any of them meets a marker boundary
  that the fix changes was not checked.
- `lambo serve-web` and its #247 lazy embedder. It is not part of this gate.
- The PostgreSQL (#323) and Elasticsearch (#321) changes. This rig's store is
  SQLite and it has no Elasticsearch tier.
- The absolute latency level and its cause. It is higher than the 0.15.0
  gate's under a heavier machine load (section 4.3); it is not attributed to
  either build and was not investigated further.
- The dresscode rig, which runs the PATH binary on a separate store. It is not
  part of this gate.
