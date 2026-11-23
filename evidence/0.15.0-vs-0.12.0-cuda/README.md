# 0.12.0 and 0.15.0 on the CUDA rig: the re-pin gate

The RTX 4070 SUPER dogfood rig's run of the re-pin gate for the step from
0.12.0 to 0.15.0. The last gate on this rig is
[0.12.0 and 0.8.0](../0.12.0-vs-0.8.0/README.md) (2026-11-17). This one follows
the method of the Metal gate
[0.14.0 and 0.15.0](../0.15.0-vs-0.14.0-metal/README.md): the same harnesses
and arguments, the same-build recall control, the paired and alternating
16-agent runs, and the same helper scripts retargeted to the two builds. Because
the step spans three releases, the change section covers 0.13.0 and 0.14.0 as
well as 0.15.0.

- **When:** 2026-11-22. Times below are UTC.
- **Builds:** OLD is 0.12.0 (binary `lambo-0.12.0`, `lambo 0.12.0`, merge commit
  a784564e, built with `--features store-sqlite,embed-candle-cuda,embed-bge`).
  The candidate is 0.15.0 (tag `v0.15.0`, merge commit 8f5fdd07), built from
  source with the same features. There is no CUDA release asset. The build
  command, its two environment fixes and the first failed attempt are in
  section 7. The live 0.12.0 writer stayed up on the same GPU throughout the
  gate.
- **Machine:** RTX 4070 SUPER, 12 GB, CUDA 13.5 (nvcc V13.5.37), the same rig
  as the previous CUDA gate. Both gate builds embed with candle BGE-M3 on CUDA
  (1024-d). This is a different rig from the Metal gate: M3 Pro, Metal, a
  2,743-concept largest session with a `general` layer, against this rig's
  11,270-concept largest session and no attached layer. No figure from one rig
  is combined with a figure from the other.
- **Store:** a SQLite `.backup` of the serving store taken at 18:48 while the
  writer was live: 11,295 concepts across 5 sessions. The gate ran against the
  largest session, `lambo-dev` (11,270 concepts, 27,253 edges, 3,896
  interactions), with no layer attached, as in the live config. Every run used
  its own fresh copy, provisioned by the build that served it. The counts are
  from the snapshot, not in a file here; the replay file's `store.base` holds
  the post-truncation counts.
- **Pending writes:** the snapshot's 19 `write_intents` rows were all applied;
  none had `consumed_at IS NULL` (from the snapshot, not in a file here). The
  replay removed the 19 with the truncated history
  (`meta.truncation.write_intents`), and every concurrent run ends with
  `write_queue_replay_owed` and `write_queue_replayed` at 0 (`final_stats`).
- **Provisioning:** 0.12.0's provision reported the keys already followed its
  #148 rule and left `store_meta.canonical_key_rule` at
  `148-identifiers-unstemmed`. 0.15.0's provision re-keyed 0 concepts and moved
  the marker to `301-empty-stems-dropped`. Both reported an idempotent schema
  provision with `schema_version` 1; no schema text changed. From the provision
  logs and a comparison of the copies, not in a file here.
- **Promotion policy:** the live `lambo.toml` has no `promotion_policy`, and the
  unit's environment sets none; the live heartbeat after the re-pin reports
  `promotion_policy = Swarm`. The gate serves ran Swarm too. Checked on the
  rig; not in a file here.

These files hold no query text and no concept text from the store, and no
absolute paths. The recall probe produced no warning lines at all on this rig,
so `probe-recall.txt` carries counts, id prefixes, scores, concept types and
session names only.

- The harness files (`sequential-ab-*.json`, `conc*.json`, `pair*.json`) and
  `probe-recall.txt` are as captured and hold aggregates, except that
  `probe-recall.txt` lists the hits of queries by node id prefix, score,
  concept type and session.
- `replay-0120-0150.json` has one row per replayed call: its kind, outcome
  counts and timings, and for the 300 real calls the id of the agent that made
  the original write and the interaction's id, both as stored. Its three path
  fields (the two binaries and the snapshot) are reduced to file names; nothing
  else in it is edited.
- `timing-stats-load.json` and `timing-canon-dryrun.json` are hyperfine 2.0.0
  exports (raw `measurements`, because that version exports no summary block).
  Their `command` fields are reduced to file names; nothing else is edited.
  `stats-session-0120.txt` and `stats-session-0150.txt` are the output of the
  timed `lambo stats` command, one per build.
- `canon-dryrun-0120.json` and `canon-dryrun-0150.json` are the two
  `lambo canon dry-run --json` reports, with `source.path` reduced to its file
  name. They hold counts and full concept ids, and no text.
- `ledger-summary.json`, `ab-ledger-top8.json` and `probe-top8.json` are
  computed by the scripts beside them, from each serve's own call ledger and
  from the probe's saved recall bodies. Those inputs hold memory text and stay
  on the rig. The scripts are the Metal gate's, with the build tags in their
  docstrings changed.

## What 0.13.0, 0.14.0 and 0.15.0 change

From the `## 0.13.0`, `## 0.14.0` and `## 0.15.0` sections of `CHANGELOG.md`.
This step spans three releases, unlike the Metal gate's one. Inside a serving
writer:

- **Recall strength (0.13.0).** Every recall reports `top_legs` (`bm25`,
  `vector_cosine`, `exact_name_match`) and a provisional `confidence` object,
  additive to the result and to the ledger line; no hit, ranking or threshold
  changed. A dependency question whose traversal answer was declined because a
  write changed the graph in between now runs the vector leg it skipped once
  and answers from the graph as it then is (#263). Calibration rows record the
  corpus size their constants were measured on (#254).
- **Recall result fields (0.14.0).** Recall ledger lines carry
  `concept_count` and `layer_concept_counts` (#257); hybrid writers warn when
  text concepts have lacked vectors for 60 seconds (#249); `lambo_inspect`
  reports a focus concept's type and how the focus resolved (#251). MCP tool
  schemas inline the `concept_type` enums (#250). These are diagnostics and
  tooling, not ranking changes.
- **Writes (0.15.0).** A write that meets an embedder outage in session is held
  and answers `pending_replay`, the agent's later writes wait behind it, and
  the worker retries the oldest on a backoff (#295). The replay at attach keeps
  each agent's writes in admission order and no longer trusts the wall clock
  for it (#295), and settles a durable intent the graph refuses instead of
  stopping (#294). A lane whose worker panicked gets a new worker.
  `write_queue_failed` no longer counts outages, and
  `write_queue_replay_blocked` is no longer `null` while intents are owed.
- **Lease and flush (0.15.0).** A writer whose heartbeat re-takes a lost lease
  is fenced, and a stale flush is terminal (#293). A store error no retry can
  fix degrades the session at once, a value the store rejects is dead-lettered,
  and the lease read and stats write after a failed flush run under the flush's
  timeout (#301).
- **Load at attach (0.15.0).** The load check tests `Causal`/`Dependency`
  acyclicity and `Hierarchical` acyclicity in two separate passes, so a session
  with a mixed cycle loads (#325).
- **Canonical keys (0.15.0).** An empty stem no longer reaches a key (#301).
- **Recall ranking (0.15.0).** Zero-score ties under RRF and the default max
  fusion fall to the tie-break (#328); an exact durable-name match keeps its
  BM25 when that is above 1.0 (#288); a dependency-question phrase routes to
  traversal only as whole words (#299); hits carry durable names (#285).
- **Daemon and canonization (0.15.0).** Conflict detection keeps an edge's
  first writer (#297); the hot list is synced once per cycle (#298); Solo
  canonization dates a dependence by the turn that wrote it (#300); replacement
  candidates are listed at their highest cosine (#302).

The rest is outside the serve loop: `lambo embed` and `lambo stats --store`
(#258, #248), `ledger_drift.py` (#177), store-wide session discovery in
`lambo serve-web` (#276), `lambo re-embed` (#296, #322), `--auth-token` parsing
(#315), the Lean models and property tests (#291), and the observability
scripts (#281). None is exercised by this gate.

## Summary

Latency rows are client-side from the harness files unless marked (server),
which are from `ledger-summary.json` and leave out each run's first 5 s
(definition in section 4).

| Test | 0.12.0 | 0.15.0 | Verdict |
|---|---:|---:|---|
| Replay of 300 real writes and 30 variants | 330 applied | 330 applied | PASS; 0 of 330 calls differ (section 1) |
| Top-8 recall on the 20 A/B queries | | | identical ids in the same order on all 200 pairs; `recall_topk_jaccard` mean 1.000 (section 2) |
| Same 20 queries, two OLD serves and one 0.15.0 | | | mean Jaccard 1.000 between builds and 1.000 between the two 0.12.0 serves (section 2) |
| Sequential recall p50 / p99 (server) | 11.05 / 49.72 ms | 11.09 / 47.35 ms | within 0.04 ms at p50, one run |
| Sequential derive ack p50 / p90 (server) | 0.10 / 27.98 ms | 28.72 / 52.76 ms | **gap in this one run; both sides show the same bimodal slow path, no cause established** (section 2) |
| Sequential apply lag p50 / p90 (server) | 41.57 / 78.47 ms | 41.46 / 43.51 ms | p50 level |
| 8 agents: writes failed | 0 / 934 calls | 0 / 933 calls | none failed on either |
| 8 agents: recall p50 / p99 (server) | 13.39 / 83.40 ms | 13.27 / 81.79 ms | level, one run each |
| 8 agents: derive ack p50 (server) | 0.11 ms | 0.11 ms | level |
| 16 agents, single runs: writes failed | 0 | 0 | none failed on either |
| 16 agents, single runs: recall p50 / p99 (server) | 37.12–42.34 / 118.25–121.11 ms | 40.56–44.10 / 123.35–129.05 ms | p50 close; p99 higher on 0.15.0 in the alternating runs (section 4.1) |
| 16 agents, four pairs: recall p99 (server) | 143.75–158.75 ms | 144.03–149.81 ms | ratios 1.03 / 0.98 / 0.97 / 1.02; no consistent lean (section 4.2) |
| 16 agents, four pairs: slow calls (server) | 364, 359, 334, 407 | 357, 445, 396, 432 | higher on 0.15.0 in three of four pairs; see the tail comparison (section 4.3) |
| Writes failed, all 16 concurrent/paired runs | 0 | 0 | none failed on either |
| Session load, `lambo stats --session lambo-dev`, 30 runs, mean | 826.4 ms | 832.9 ms | 0.8% higher on 0.15.0 (section 5) |
| Canonization dry run, 8 runs, mean | 27,150.8 ms | 26,966.7 ms | 0.7% lower on 0.15.0 (section 5) |
| Canonization dry-run reports | | | Swarm identical; Solo 498 against 496 reach Candidate (section 5) |

**Verdict: the gate passes and the rig was re-pinned to 0.15.0 the same day
(section 6).** Replay is identical (0 of 330 calls differ). The ranked top 8
is identical on all 200 A/B pairs and in the same-build probe, where both
between-build and same-build Jaccard are 1.000. No write failed in any of the
16 concurrent or paired runs, and no harness reported an error or a dead
worker. Medians are level in the 8-agent, 16-agent, paired and alternating
runs and in both timings; the one exception is sequential derive ack, where
this 30-call A/B sample splits 6/30 against 24/30 between a ~0.1 ms fast path
and a 10–94 ms slow path, while the 8- and 16-agent runs split almost exactly
the same way on both builds (23.4%/23.7% and 74.5%/75.3% above 10 ms). Both
sides have the slow path, its own latency is no worse on 0.15.0, and no cause
for the different split was established. The 16-agent upper-tail lean seen on
the Metal rig does not reproduce as a recall p99 gap here (section 4.3).

## 1. Replay parity

**Method:** `replay_parity.py`, the last 300 real write calls
of session `lambo-dev` (125 derive, 175 record_action) plus 30 reworded
variants, with `--canon-eval-secs 100000000 --canon-wait 0`. The built-in
reason catalogue needed no new entries. Because the snapshot is from this rig
and not the Metal rig's, these are different calls from the Metal gate's
replay. The truncation removed 1,411 edges, 560 concepts, 307 interactions,
the 19 write intents and the 5 lease rows (`meta.truncation`).

File: [`replay-0120-0150.json`](replay-0120-0150.json).

**0 of 330 calls differ.** Both sides applied 330 of 330, created 529
concepts, semantically merged 26, embedded all 529, and wrote 444 new edges.
Both ended on stores with 11,264 concepts, all embedded, 27,309 edges with the
same count per edge type, 3,943 interactions, and the same canonization status
counts (1,286 Candidate, 4 Canonical, 10 Venerable). No created concept and no
semantic edge exists on one side only. 48 calls carry reconstruction warnings.

## 2. Sequential latency A/B

**Method:** `mcp_ab.py ab`, two HTTP serves side by side,
10 rounds of 20 queries interleaved per query, then 30 derives per side each
followed by a receipt wait. 0.12.0 is listed first and was started first. The
script waited 90 s after both serves answered before starting the harness.
Both serves ran with the default rate limit; neither side retried a
rate-limited request (`rate_limited_retries` 0/0), and neither reported an
error.

File: [`sequential-ab-0120-0150.json`](sequential-ab-0120-0150.json).

The latency figures are in the summary. `recall_topk_jaccard` has mean 1.000
over the 20 queries: every query returned the same set on both builds.

### What the Jaccard is

[`ab-ledger-top8.json`](ab-ledger-top8.json), computed by
[`ab_ledger_top8.py`](ab_ledger_top8.py) from the two serves' ledgers, pairs the
two sides' 200 recalls. The first eight hits are the same ids in the same order
on all 200 pairs, with a score difference of 0.0 everywhere. Unlike the Metal
gate, no ninth hit appears on either side (`first_round_set_overlap_mean`
1.000).

### The same-build control

**Method:** [`probe_recall.py`](probe_recall.py) asks the same 20 queries, with
the same warm-up and `top_k = 8`, of three serves on fresh copies: two
instances of 0.12.0 (`0120a`, `0120b`) and one of 0.15.0, all answering at
14:07:03. Three rounds; the script waited 60 s after the last serve answered.
It ran after the 16-agent runs.

Files: [`probe-recall.txt`](probe-recall.txt) and
[`probe-top8.json`](probe-top8.json), computed from its saved bodies by
[`probe_top8.py`](probe_top8.py).

- The first eight hits are the same on all three serves, same ids in the same
  order with score difference 0.0, for every query in every round.
- Mean Jaccard is 1.000 between 0.12.0 and 0.15.0 and 1.000 between the two
  0.12.0 serves. No response has a ninth hit and no response carries a warning
  line.
- The Metal gate's ninth idle-session notice hit does not appear on this rig.
  The same-build control therefore has nothing to disagree about here; it is
  still the control that pays off the CUDA A/A run the 0.12.0 gate left owed.

### The sequential derive-ack gap

Both sides' derive acks are bimodal: ~0.1 ms on the fast path and 10–94 ms on
the slow path. In this run 0.12.0 took the slow path on 6 of 30 derives and
0.15.0 on 24 of 30, all inside the same 7–10 s window after the first call.
0.12.0's slow path reached 93.89 ms and 0.15.0's 59.26 ms. In the 8-agent runs
the split is 23.4% against 23.7% above 10 ms, and in the 16-agent runs 74.5%
against 75.3%; their server-side ack medians are 0.11/0.11 ms and
36.04/37.02 ms. The gap is confined to this 30-call sample and no cause for
the different split was established. Apply lag, which times the actual write,
has a level p50 (41.57 against 41.46 ms).

## 3. Concurrent agents

**Method:** `mcp_concurrent.py` for 120 s, default mix, seed 1;
8 workers with 1.0 s think time and 16 with 0.5 s. One serve at a time, each on
a fresh copy, default rate limit, 0.12.0 first. Every run drained, every call
succeeded and no worker died. The two 8-agent runs end with 12,219 and 12,217
concepts, all embedded.

Files: `conc-<build>-w<N>.json`.

| Run | Calls | Failed writes | Recall p50 / p99 (server) | Ack p50 (server) | Apply lag p50 / p90 (server) | Slow calls |
|---|---:|---:|---|---|---|---:|
| `conc-0120-w8` | 934 | 0 | 13.39 / 83.40 ms | 0.11 ms | 61.91 / 128.74 ms | 0 |
| `conc-0150-w8` | 933 | 0 | 13.27 / 81.79 ms | 0.11 ms | 67.88 / 113.81 ms | 3 in 3 s |
| `conc-0120-w16` | 3,484 | 0 | 41.32 / 121.11 ms | 36.04 ms | 99.15 / 155.07 ms | 108 in 46 s |
| `conc-0150-w16` | 3,477 | 0 | 41.47 / 118.84 ms | 37.02 ms | 104.16 / 155.27 ms | 139 in 58 s |

Client-side recall p50 / p99 is 14.91 / 84.48 ms against 14.91 / 115.63 ms at
8 agents and 47.41 / 125.70 ms against 49.42 / 125.61 ms at 16. The server-side
figures, which exclude each run's first 5 s, are the comparable ones here. On
this rig a "slow call" is common under 16 agents because the 100 ms threshold
sits near the store's ordinary p90; both sides have hundreds of them.

## 4. The 16-agent runs

The figures in this section are server-side, from each serve's own ledger
([`ledger-summary.json`](ledger-summary.json), computed by
[`ledger_summary.py`](ledger_summary.py), which states the definitions). A
**slow call** is a recall or a write ack of 100 ms or more that ended more than
5 s after the run's first call. "N in M s" is N slow calls ending in M distinct
clock seconds.

### 4.1 Six single runs

| First call | Build | Files | Recall p50 / p90 / p99 / max (ms) | Ack p99 / max (ms) | Apply lag p50 / p90 / p99 (ms) | Slow calls |
|---|---|---|---|---|---|---:|
| 13:32:39 | 0.12.0 | `conc-0120-w16` | 41.32 / 86.92 / 121.11 / 167.77 | 116.65 / 199.74 | 99.15 / 155.07 / 261.92 | 108 in 46 s |
| 13:38:05 | 0.15.0 | `conc-0150-w16` | 41.47 / 89.30 / 118.84 / 175.66 | 120.99 / 199.89 | 104.16 / 155.27 / 254.17 | 139 in 58 s |
| 13:53:41 | 0.15.0 | `conc3-r1-0150-w16` | 40.56 / 88.15 / 123.35 / 196.55 | 130.49 / 281.73 | 96.81 / 150.47 / 262.40 | 141 in 60 s |
| 13:56:43 | 0.12.0 | `conc3-r2-0120-w16` | 37.12 / 86.66 / 119.61 / 135.93 | 119.82 / 234.30 | 99.34 / 157.29 / 254.66 | 111 in 45 s |
| 13:59:48 | 0.15.0 | `conc3-r3-0150-w16` | 44.10 / 89.58 / 129.05 / 215.67 | 129.36 / 187.06 | 99.77 / 159.15 / 263.54 | 132 in 56 s |
| 14:02:51 | 0.12.0 | `conc3-r4-0120-w16` | 42.34 / 85.53 / 118.25 / 181.41 | 122.20 / 202.87 | 105.35 / 150.13 / 246.65 | 117 in 41 s |

- Recall p50 is between 37.12 and 44.10 ms in all six runs and apply lag p50
  between 96.81 and 105.35 ms.
- The four alternating runs started 0.15.0 first, then 0.12.0, then 0.15.0,
  then 0.12.0. The 1-minute load averages logged immediately before them are
  1.51, 0.78, 1.49 and 1.09 in run order; slow-call counts are 141, 111, 132
  and 117. Nothing was sampled during a run.
- No call failed and no write failed in any of the six.

### 4.2 Both builds at the same time

**Method:** one serve of each build, each on its own fresh copy and port, each
under its own `mcp_concurrent.py` with the 16-agent arguments, started together
15 s after both serves answered. Whatever else the machine does reaches both
at once. Two serves were under load at once with the live writer on the same
GPU. Apply lag p50 is 62–79 ms in the paired runs, lower than the 96–105 ms in
the single runs; nothing in these runs establishes why. Only the two sides of
a pair compare. Pairs 1 and 3 start 0.12.0 first; pairs 2 and 4 start 0.15.0
first. Each pair's first calls are at the same second on both sides.

Files: `pair<N>-<build>-w16.json`, and the `pairs` block of
`ledger-summary.json`, which compares the two sides second by second over the
115 s after both have warmed up.

| | Pair 1: 0.12.0 | Pair 1: 0.15.0 | Pair 2: 0.12.0 | Pair 2: 0.15.0 |
|---|---:|---:|---:|---:|
| Started | first | second | second | first |
| Recall p50 / p99 (ms) | 50.80 / 143.75 | 53.14 / 147.59 | 47.09 / 158.75 | 52.84 / 156.18 |
| Ack p99 / max (ms) | 142.11 / 187.95 | 147.86 / 292.53 | 152.13 / 206.91 | 150.82 / 240.55 |
| Apply lag p50 / p90 (ms) | 78.94 / 171.13 | 69.24 / 164.82 | 65.28 / 164.69 | 66.86 / 172.12 |
| Slow calls | 364 in 86 s | 357 in 85 s | 359 in 81 s | 445 in 89 s |
| Seconds with slowest call >= 100 ms | 86 | 85 | 81 | 89 |
| ... of them the same second on both sides | 70 | 70 | 67 | 67 |
| Seconds in which this side's slowest call is the slower by over 5 ms | 51 | 45 | 43 | 60 |

| | Pair 3: 0.12.0 | Pair 3: 0.15.0 | Pair 4: 0.12.0 | Pair 4: 0.15.0 |
|---|---:|---:|---:|---:|
| Started | first | second | second | first |
| Recall p50 / p99 (ms) | 50.18 / 148.02 | 46.71 / 144.03 | 50.53 / 147.66 | 55.40 / 149.81 |
| Ack p99 / max (ms) | 152.29 / 286.38 | 152.65 / 303.10 | 156.23 / 271.59 | 159.75 / 281.31 |
| Apply lag p50 / p90 (ms) | 66.14 / 166.23 | 67.90 / 171.15 | 75.01 / 171.51 | 62.46 / 167.46 |
| Slow calls | 334 in 80 s | 396 in 89 s | 407 in 87 s | 432 in 94 s |
| Seconds with slowest call >= 100 ms | 79 | 89 | 87 | 94 |
| ... of them the same second on both sides | 67 | 67 | 80 | 80 |
| Seconds in which this side's slowest call is the slower by over 5 ms | 38 | 58 | 47 | 54 |

- **Recall p99 does not lean consistently.** The 0.15.0 minus 0.12.0 ratios are
  1.027, 0.984, 0.973 and 1.015; each side is higher in two pairs. No pair is
  near the 2× no-re-pin threshold. The median of the per-second slowest call
  (0.15.0 minus 0.12.0) is -2.33, +5.93, +5.00 and +1.92 ms, again mixed.
- **Slow-call counts lean towards 0.15.0 in three of four pairs**, the
  exception being pair 1 (364 against 357). No second on either side of any
  pair has a call of 500 ms or more.
- No call failed and no write failed in any of the eight runs.

### 4.3 Comparing the tail with the Metal gate

The Metal gate left open a small 16-agent upper-tail lean towards 0.15.0: more
slow calls, a higher recall p99 in both pairs (by 1.09 and 4.68 ms), and a
per-second slowest call a few milliseconds higher on 0.15.0 in both pairs
(+0.86 and +1.07 ms). Over five 16-agent runs there, recall p99 ranged from
20.22 to 25.25 ms on 0.14.0 and from 21.71 to 29.93 ms on 0.15.0. It also saw
one stalled second with 10 slow calls on 0.15.0.

On this CUDA rig the p99 lean does not reproduce: the four pair ratios straddle
1.0 (section 4.2), the alternating single runs put 0.15.0 higher twice and
lower twice (section 4.1), and the per-second median difference is negative in
one pair. What does lean 0.15.0-ward here is the count of slow calls in three
of four pairs, but the absolute counts are not comparable with the Metal
gate's: a slow call is 100 ms or more, and the CUDA store's ordinary recall
p90 already sits near 100 ms, so 334–445 slow calls per pair run is common on
both sides. The Metal gate's 0 and 12 slow calls over five runs came from a
2,743-concept session on Metal, against 11,270 concepts here on CUDA. The two
rigs also differ in session layout (Metal serves eight sessions plus a `general`
layer, this rig five sessions and no layer) and in GPU (M3 Pro Metal against
RTX 4070 SUPER CUDA under a second live writer). Nothing in these runs
establishes a cause for the difference in tail behaviour between the rigs, or
for the slow-call counts within this one.

## 5. Session load and the canonization dry run

**Method:** hyperfine 2.0.0 (installed on the rig for this gate), both builds
against one more copy of the snapshot, with no serve attached, after all load
runs had finished. Both commands only read it; the dry-run reports confirm
their source was unchanged (the same sha256 before and after). The 1-minute
load average before the timings was 1.04.

- `lambo stats --config lambo.toml --session lambo-dev`, 30 runs per build,
  3 warmups. This loads the session the way a writer attach does, so on 0.15.0
  it includes the #325 two-pass cycle check.
- `lambo canon dry-run --store store.db --session lambo-dev --at
  2026-11-19T11:58:32Z --json`, 8 runs per build. The clock is one second after
  the snapshot's newest row, so both builds survey the same state. Each run
  surveys both promotion policies.

Files: [`timing-stats-load.json`](timing-stats-load.json),
[`timing-canon-dryrun.json`](timing-canon-dryrun.json),
[`stats-session-0120.txt`](stats-session-0120.txt) and
[`stats-session-0150.txt`](stats-session-0150.txt),
[`canon-dryrun-0120.json`](canon-dryrun-0120.json) and
[`canon-dryrun-0150.json`](canon-dryrun-0150.json).

| Command | Build | Runs | Mean / median (ms) | Min / max (ms) | Peak RSS, mean (MiB) |
|---|---|---:|---|---|---:|
| `stats --session lambo-dev` | 0.12.0 | 30 | 826.4 / 824.3 | 816.5 / 843.4 | 490.0 |
| `stats --session lambo-dev` | 0.15.0 | 30 | 832.9 / 831.8 | 818.1 / 856.0 | 491.8 |
| `canon dry-run` | 0.12.0 | 8 | 27,150.8 / 27,165.7 | 26,969.2 / 27,244.3 | 422.3 |
| `canon dry-run` | 0.15.0 | 8 | 26,966.7 / 27,005.2 | 26,574.8 / 27,134.1 | 430.0 |

Every run exited 0. The two `stats` outputs report the same 15,166 nodes,
27,253 edges, 11,270 concepts, all embedded, and 5 Canonical concepts; the
only textual difference is that 0.15.0's calibration line adds
`(layer floor measured on 12 concepts)` (#254). Session load is 0.8% higher on
0.15.0 and the dry run 0.7% lower; on this store both are within the run-to-run
spread of the other build.

**The dry-run reports.** In both reports every invariant held, the source
store's hash is the same before and after, and both see 11,270 concepts,
27,253 edges and 3,896 interactions. A field-by-field diff differs only inside
the Solo policy's block:

- **Swarm:** identical.
- **Solo:** both fixed point at 5 Canonical and 10 Venerable. 498 concepts
  reach Candidate on 0.12.0 and 496 on 0.15.0 (`reached.Candidate.total`;
  Entity 115 against 113), so 0.15.0 ends with two fewer Candidates (295
  against 293 Entity at `end`). The first evaluation's `stage1/solo_score`
  near-misses rise from 1,306 to 1,308, and the first cycle moves 212
  non-Resource concepts on 0.12.0 against 210 on 0.15.0.

Fewer Solo Candidates is the direction the CHANGELOG's #300 entry describes
("existing Solo Candidates and Venerables can lose score"), and Swarm is
unchanged, as that entry says. The reports list counts, not the concepts that
moved, so the gate did not identify which two concepts these are or trace them
to #300.

## 6. The re-pin

The rig was re-pinned to 0.15.0 at 14:21 (22:21 local; DOGFOOD-SETUP §2a).
None of this section is in a file here; it is from the rig's ledger, journal,
backup and MCP answers.

- The unit file was copied aside as `.bak-0.12.0`, and its `ExecStart` now names
  `lambo-0.15.0` directly with the same five sessions, agent, port, ledger and
  heartbeat interval. OLD remains installed as `lambo-0.12.0`; rollback is to
  stop the service, restore `.bak-0.12.0`, run daemon-reload and start again,
  restoring the pre-repin store backup if the live store must also go back.
- The old writer exited on the stop signal without waiting out the lease TTL,
  and released its leases. All 5 session leases were held by the new process
  (pid 258611) within a few seconds of the start.
- The pre-repin backup is `backup-pre-0.15.0-20261122T142000Z.db` with a
  `.sha256` beside it (a970ce29c80e2f1dd348cfbdda29d6d8ed0dda54aa5dc08484046cd61935cbb5).
- `provision` by 0.15.0 on the live store re-keyed 0 concepts and left the
  schema unchanged; it moved `canonical_key_rule` to
  `301-empty-stems-dropped`.
- The first ledger heartbeat after the start reports git sha 8f5fdd0, version
  0.15.0, `degraded = false`, `dead_lettered = 0` and `promotion_policy =
  Swarm`. A live recall answered with ranked hits and the additive 0.15.0
  fields.
- The writer's journal has no WARN or ERROR line since the restart. It carries
  INFO lines only, including five stale-session daemon events on idle
  sessions.

## 7. Build notes

Built from tag `v0.15.0` (8f5fdd07) in a detached worktree, with the features
0.12.0 was built with:

```sh
NVCC_CCBIN=clang CUDARC_CUDA_VERSION=13040 LAMBO_GIT_SHA=8f5fdd0 \
  cargo build --locked --release \
  --features store-sqlite,embed-candle-cuda,embed-bge
```

The first attempt, with only `NVCC_CCBIN=clang` and `LAMBO_GIT_SHA=8f5fdd0`,
failed before compiling any Lambo code: cudarc 0.19.10's build script panicked
with `Unsupported cuda toolkit version: 13.5` against the rig's nvcc V13.5.37.
cudarc 0.19.10 lists 13.4 as its newest supported toolkit. The retry pins its
bindings to 13.4 with `CUDARC_CUDA_VERSION=13040`, following the rig's earlier
ruling that CUDA 13.x minor versions are ABI compatible; this is an environment
fix, not a code change. The retry finished in 6 m 05 s. The binary was installed
as `lambo-0.15.0`, and `--version` reports `lambo 0.15.0`.

`embed-candle-cuda` is not compiled in CI, so this build is the only check that
the feature compiles at this tag on this toolkit. hyperfine was absent on the
rig and was installed from crates.io for the timings (version 2.0.0).

## Not covered

- Replay drain timing and the #295 replay order. The snapshot had no owed
  intent (all 19 consumed), so no serve had a backlog to drain:
  `write_queue_replay_owed` and `write_queue_replayed` are 0 in every
  concurrent and paired run.
- The #295 in-session hold and `pending_replay`, the #294 replay refusals and
  the reworked counters under a real embedder outage. No harness stops the
  embedder; `write_queue_outstanding` and `write_queue_failed` end at 0 and
  `write_queue_replay_blocked` at `null` in every run.
- The #293 lease fencing. No takeover was staged; the only lease handover seen
  is the live re-pin (section 6).
- The #301 flush and store-error paths. No store error occurred
  (`dead_lettered = 0` and `degraded = false` in every `final_stats`). The
  re-key changed no key in this store, and no harness write was checked for an
  empty-stem token.
- A session holding a mixed `parent_of`/citation cycle (#325). All five
  sessions of the snapshot load on 0.12.0, so none holds one; section 5 times
  the second pass on a session without one.
- The recall changes (#328, #288, #299, #285) as such. The 20 queries' ranked
  hits are identical on both builds, but whether any of them meets a zero-score
  tie, an exact durable-name match or a dependency phrase was not checked, and
  no harness reads a `names` field.
- The daemon changes (#297, #298, #302). No harness reads conflict warnings,
  the hot list or replacement candidates; `probe-top8.json` counts warning
  lines only.
- Solo canonization in a live serve (#300). Only the dry run covers it, and the
  rig runs Swarm.
- Store-wide session discovery and its listing in `lambo serve-web` (#276).
- `lambo re-embed` (#296, #322), `--auth-token` parsing (#315), the
  Elasticsearch recall tier (#319) and `lambo_inspect` with durable names
  (#285).
- Ledger heartbeats under load. The gate serves ran with `--ledger` and without
  `--ledger-heartbeat`, as in the Metal gate. The live writer's heartbeat is
  the one reported in section 6.
- The cause of this rig's sequential derive-ack split (section 2), of the
  slow-call counts in section 4.2, and of the difference in upper-tail
  behaviour between the two rigs (section 4.3).
- The dresscode rig, which runs the PATH binary on a separate store. It is not
  part of this gate.
