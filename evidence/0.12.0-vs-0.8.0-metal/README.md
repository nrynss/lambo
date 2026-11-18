# 0.8.0 and 0.12.0 on the Metal rig: the re-pin gate, and an ack-window probe

The Apple-silicon dogfood rig's run of the re-pin gate for the jump from
0.8.0 to 0.12.0, and a follow-up probe into why its write latencies differ
from the CUDA rig's. The CUDA rig's gate for the same jump is
[0.12.0 vs 0.8.0](../0.12.0-vs-0.8.0/README.md). **Do not mix the two rigs'
figures:** the machines, stores and session sizes differ.

- **When:** 2026-11-18.
- **Builds:** both are published release assets, `macos-arm64-metal`: 0.8.0
  (tag `v0.8.0`, 15585529, the rig's pinned binary) and 0.12.0 (tag
  `v0.12.0`, 015d4acc), checksum-verified by `install.sh`.
- **Machine:** Apple M3 Pro, 18 GB, macOS 26.7.3. Every build embeds with
  candle BGE-M3 on Metal.
- **Store:** a SQLite backup of the serving store taken while the writer was
  live: 7,259 concepts and 2,943 interactions across 8 sessions. The gate
  ran against the largest session, `lambo` (2,663 concepts, 1,023
  interactions), with the `general` layer attached for the HTTP runs as on
  the live rig. Every run used its own fresh copy, provisioned by the build
  that served it. The live writer stayed up on the same GPU throughout.
- **Provisioning:** 0.12.0 adds the `concept_names` and `retired_aliases`
  tables and nothing else.

These files hold aggregates only, with no memory content and no local paths.

## Summary

| Test | 0.8.0 | 0.12.0 | Verdict |
|---|---:|---:|---|
| Replay of 330 real writes | 330 applied | 330 applied | PASS, 0 unexpected, matching final store counts |
| Top-8 recall on the 20 A/B queries | | | identical on all 20 (Jaccard 1.0) |
| Sequential recall p50 / p99 | 1.99 / 22.88 ms | 2.00 / 22.62 ms | within 0.3 ms, one run |
| Sequential `lambo_stats` p50 | 0.44 ms | 0.39 ms | within 0.05 ms, one run |
| Sequential derive ack p50 / p90 | 0.50 / 0.58 ms | 0.50 / 0.58 ms | equal, one run |
| Sequential derive applied p50 / p90 | 25.49 / 26.05 ms | 25.42 / 26.05 ms | within 0.1 ms, one run |
| 8 agents: writes failed | 0 / 546 | 0 / 544 | none failed on either |
| 8 agents: recall p50 / p99 | 9.38 / 38.63 ms | 10.20 / 39.38 ms | close, one run each |
| 8 agents: derive ack p50 | 2.04 ms | 2.38 ms | close, one run each |
| 8 agents: apply p50 / p90 (server) | 49 / 62 ms | 52 / 66 ms | close, one run each |
| 16 agents: writes acked / failed | 2,129 / 0 | 2,128 / 0 | none failed on either |
| 16 agents: recall p50 / p99 | 5.74 / 30.73 ms | 5.65 / 24.51 ms | close at p50, 0.12.0 lower at p99, one run each |
| 16 agents: derive ack p50 | 0.77 ms | 0.79 ms | close, one run each |
| 16 agents: apply p50 / p90 (server) | 61 / 119 ms | 56 / 118 ms | close, one run each |
| `names migrate` preview, 8 sessions | | 0 changes in each | provision-only upgrade |

**These runs show no regression and no speed-up.** The sequential A/B
figures of the two builds are within 0.3 ms of each other. The concurrency figures are close, with
0.12.0 a few percent slower at 8 agents (derive ack p50 2.04 against
2.38 ms) and level or slightly faster at 16. Every cell is one run, so
these runs do not establish whether a difference of that size is noise.

The rig was re-pinned to 0.12.0 the same day (DOGFOOD-SETUP §2a, writer down
for about 5 seconds, all 8 sessions re-attached, every concept embedded).

## 1. Replay parity

**Method:** `scripts/loadtest/replay_parity.py`, the last 300 real write
calls of session `lambo` (124 derive, 176 record_action) plus 30 reworded
variants, with `--canon-eval-secs 100000000 --canon-wait 0`. The built-in
reason catalogue needed no new entries.

File: [`replay-080-0120.json`](replay-080-0120.json).

Both sides applied 330 of 330, created 610 concepts, semantically merged 30,
and ended on stores with matching counts: 5,870 concepts, all embedded,
14,525 edges with the same count per edge type. The stores are not
identical: 14 semantic edges exist only on the 0.8.0 side and 14 only on the
0.12.0 side, which is the merge-target difference below. 34 calls differ, all
`merge_into_replayed_concept` (merge-target noise, as in earlier gates);
0 are unexpected.

## 2. Sequential latency A/B

**Method:** `scripts/loadtest/mcp_ab.py ab`, two HTTP serves side by side,
10 rounds of 20 queries interleaved per query, then 30 derives per side each
followed by a receipt wait. 0.8.0 is listed first.

File: [`sequential-ab-080-0120.json`](sequential-ab-080-0120.json).

**The first attempt failed in the harness.** Sequential recalls on this rig
run at about 100 a second, which trips the scratch serve's default limit of
50 requests a second per credential, and `mcp_ab.py` does not retry an
HTTP 429. The run reported here started both scratch serves with
`--rate-limit-rps 0`. The concurrency harness retries 429s and was not
affected.

## 3. Concurrent agents

**Method:** `scripts/loadtest/mcp_concurrent.py` for 120 s, default mix,
seed 1; 8 workers with 1.0 s think time and 16 with 0.5 s. One serve at a
time, each on a fresh copy, default rate limit. Every run drained and no
worker died. At 16 agents 2,000 of the acked receipts were queried: that is
the harness's `--receipt-max` sample cap (`"sampled": true`), not lost
writes.

Files: `conc-<build>-w<N>.json`.

## 4. Names preview

**Method:** `lambo names migrate --session <S>` (preview: no lease, no
write) on a copy provisioned by 0.12.0, once per session.

File: [`names-preview.json`](names-preview.json), counts per session with
the session names left out. All 8 sessions: 0 names migrated, 0 aliases
retired, 0 bindings, 0 retirements.

## 5. Why writes look different from the CUDA gate

The CUDA gate reports a derive ack of 28.57 ms on 0.8.0 against 5.58 ms on
0.12.0, and derive applied of 73.67 against 44.74 ms. That write-up labels
the second pair "ack to applied", but `mcp_ab.py`'s `derive_applied` timer
starts when the derive is sent, so it is submit to applied and includes the
ack. Here both builds ack in 0.50 ms and reach applied in about 25 ms. This section is a probe into that
difference. **Its conclusion about the CUDA figures is a hypothesis and
needs a run on the CUDA rig.**

### What was measured here

A derive's ack takes the graph write lock
(`src/writeq/admission.rs`, `admit`). The probe
([`ack_window.py`](ack_window.py)) drives one serve alone, with no
interleaving: derive, wait for the receipt to read `applied`, pause, derive
again. It records the ack latency of the next derive for each pause length,
25 derives per pause, the first of each group dropped.

Two stores: the split snapshot's `lambo` session (2,663 concepts) and a copy
of the archived pre-split store, one session of 5,599 concepts with no layer.

| ack p50 (ms) after a pause of | 0 ms | 5 ms | 10 ms | 20 ms | 40 ms |
|---|---:|---:|---:|---:|---:|
| 2,663-concept session, 0.8.0 | 0.57 | 1.49 | 0.73 | 0.88 | 1.13 |
| 2,663-concept session, 0.12.0 | 0.52 | 0.71 | 0.92 | 0.99 | 1.12 |
| 5,599-concept session, 0.8.0 | 13.68 | 9.60 | 0.69 | 0.75 | 1.04 |
| 5,599-concept session, 0.12.0 | 14.27 | 7.96 | 0.72 | 0.72 | 1.01 |

Files: `ack-window-<store>-<build>.json` (`split` is the 2,663-concept
session, `single` the 5,599-concept one), which also carry p90, max and the
receipt-wait p50 (about 25 to 35 ms in every cell).

Findings:

- **There is a slow window right after a write applies**, during which the
  next ack waits. It was about 14 ms long on the 5,599-concept session and
  not visible at p50 on the 2,663-concept one (it shows there only at p90,
  7 to 9 ms with no pause).
- **The window grew with session size**, on two sizes. Two points do not
  give a scaling law.
- **The window measured about the same on 0.8.0 and 0.12.0** (13.68 and
  14.27 ms with no pause, one run each). The write-queue lock paths
  are also unchanged between the tags: the `src/writeq` diff adds the names
  payload variants and a legacy-pin refresh, and nothing that changes when
  the graph lock is held.

### Why this rig is faster

- Its sessions are smaller. The store is split into 8 sessions and the gate
  ran on one of 2,663 concepts; the CUDA store is one session of 11,218.
- Its write cycle (about 26 ms) is longer than its window, so in the
  interleaved A/B the next ack always lands after the window. Both builds
  therefore show the floor, 0.50 ms.
- Not separated out: raw hardware and disk differences, and the CUDA rig's
  live writer being busy on the same GPU during its run.

### Hypothesis: the CUDA speed-up is an interleave artifact

`mcp_ab.py` runs, per derive: first side's derive and receipt wait, then the
second side's. So the first side's next derive comes one *second-side* cycle
after its own apply, and the second side's comes one *first-side* cycle
after. If the window on an 11,218-concept session is longer than the faster
side's cycle, the first-listed side lands inside it every time and the
second lands outside. That is a stable asymmetry between two identical
builds.

What supports it:

- The CUDA figures are consistent with it. Nearly all of the reported
  applied gain is the ack term: applied minus ack is 45.1 ms on 0.8.0 and
  39.2 ms on 0.12.0 at p50, against 73.67 and 44.74 for applied itself.
  0.8.0 was listed first.
- The lock path did not change between the tags, and on this rig the two
  builds measured alike with and without the window, in one run each.
- 0.8.0's own ack p50 has moved between CUDA gates with its partner and
  position: 15.5 ms (second, against 0.6.0), 20.1 ms (second, against
  0.7.0), 28.57 ms (first, against 0.12.0).

What does not fit, or is unknown:

- In the 0.8.0 gate, 0.7.0 was listed first against 0.8.0 and acked in
  1.5 ms, the opposite of what position alone predicts. The window may
  differ by version below 0.8.0; that was not measured.
- The background pass that holds the lock in the window was not identified.
  The flusher's locked sections are brief and do not scan the session.
- The window was not measured at the CUDA store's size or on its hardware.

### What would settle it, on the CUDA rig

1. An A/A run: `mcp_ab.py ab` with the same binary on both sides. A gap
   between the sides is the artifact.
2. The same 0.8.0 and 0.12.0 pair with the sides swapped. If the slow ack
   follows the first position, not the version, it is the artifact.
3. `ack_window.py` against one serve on a copy of that store, to read the
   window's length directly.
