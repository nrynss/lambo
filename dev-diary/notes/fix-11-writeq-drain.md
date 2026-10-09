# #11 write-queue drain, and #16 §3 flush lag

Branch `fix/11-writeq-drain`, cut from main `b0e8207` (2026-10-09).

## What #11 reported

On the Metal rig the write queue's startup probe read 10.6 items/s while the
observed serial rate settled at 0.5 to 2.8 (4x to 20x apart), and a 3 to 4
concept derive's apply latency reached 4.6 s against a 4 s `RECEIPT_WAIT_MAX`,
so a read-your-writes wait could answer `pending` about a healthy write.

## Re-measurement on main (after #8)

Harness: `lambo serve` over stdio, release build with
`embed-candle-metal,store-sqlite`, on the M3 Pro. Scratch SQLite store,
private runtime dir, never the live writer. The embedder is the real candle
Metal BGE-M3, not a fixture with an injected delay. Workload: concepts drawn
to the Metal rig's distribution (half 120 to 300 B, 40% 300 to 950 B, 10% 950
to 2,000 B) and 1 to 4 concepts per derive (mean 2.2). Each run is 40
sequential derives (each waited to applied) and then a burst of 40 derives
from one agent. The driver lives in the orchestrator's scratch directory, not
in the repo.

| run | probe serial | observed serial | ratio | burst writes/s | sequential apply p50 / p90 / max | 3 to 4 concept p90 | idle `flush_lag_ms` |
|---|---|---|---|---|---|---|---|
| main, seed 11 | 11.55 | 2.80 | 4.1x | 2.69 | 0.26 / 0.69 / 1.65 s | 0.88 s | 2,497 |
| main, seed 12 | 11.40 | 2.30 | 5.0x | 2.89 | 0.20 / 0.69 / 1.41 s | 1.00 s | 2,925 |
| branch, seed 11 | 5.70 | 2.84 | 2.0x | 2.67 | 0.26 / 0.68 / 1.64 s | 1.18 s | 85 |
| branch, seed 12 | 5.74 | 2.30 | 2.5x | 2.91 | 0.20 / 0.77 / 1.45 s | 1.15 s | 71 |

The same harness with the fixture embedder applies a derive in 0.5 ms
(p50). After #8 the non-embed work of a write is negligible, so the whole gap
is the embed. The #40 implementer's 0.15 writes/s with workers waiting on the
SQLite vector query was #8's bottleneck, and it is gone.

### Where a write's time goes

A least-squares fit over the sequential derives gives about **0.12 s per KiB
of embedded text** on this rig. A hybrid derive embeds each new concept as
`content — prompt`, where the prompt is every concept of the call joined by
`"; "` (`hybrid::context_text`, `hybrid::derive_prompt`). So a `k`-concept
derive embeds about `(k + 1)` times its concept bytes, and its cost grows
roughly as `k²`.

The probe timed one bare embed of 1 KiB. The observed rate is one sample per
lane job, which is a whole write. So `probe_optimism` divided an embed rate by
a write rate. Most of the 4x to 20x was this unit mismatch. The rest was
input length (the 35 B and 1 KiB legs against contexts of 1 to 8 KiB) and
Metal's per-token cost (#8 comment).

### Batching: not done

The issue suggested batching a derive's embeds into one forward. The probe's
own 4-wide leg reads 11.59 items/s against 11.55 serial on Metal, so one
~1 KiB input already saturates the GPU and batching would not raise this
rig's drain rate. Lanes run in parallel, one worker per agent, and the
aggregate is bounded by the same saturated embedder. A CUDA rig or a
llama-server with parallel slots may differ; that is part of #11's CUDA
acceptance item.

### Admission: unchanged

The bounds are static (lane 64, queue 1024, 16 MiB) and since the J3
redesign they size memory and fairness, not durability: every accepted write
is a durable intent. At 2.7 writes/s a full lane is about 24 s of work.
Nothing measured argues for moving them. A deeper lane costs apply latency
and close deferral, both visible now (`write_queue_apply_ms_*`,
`write_queue_deferred`).

## Decisions

1. **The probe measures a representative write** (`fix(writeq): probe a
   representative write...`). `PROBE_WRITE_CONCEPTS = 2` concepts of
   `PROBE_CONCEPT_BYTES = 339`, each embedded through `hybrid::context_text`
   over `hybrid::derive_prompt`, so the probe and the derive path cannot
   drift. Two because 1 to 2 concepts is the commonest derive on the rig's
   ledger (48 of 71). 339 B because it is the largest size that keeps every
   embedded context within `PROBE_TEXT_BYTES = 1024`, the input-ceiling
   bound J3-R2-1 measured on llama-server. That keeps the leg answerable
   wherever the old one was. The concurrent leg runs `PROBE_CONCURRENCY`
   such writes at once. The ratio left over (2.0x to 2.5x) is the gap
   between a median write and the mean write the EWMA tracks, since cost is
   quadratic in `k`. That is real workload information, which is what the
   ratio is for.
2. **The warm-up has its own budget** (`PROBE_WARMUP_BUDGET =
   HYBRID_IO_TIMEOUT`). In the first smoke run on a cold page cache the
   candle Metal first embed outran the shared 5 s, and the session had no
   probe figure at all.
3. **Rates are clamped as documented.** `PROBE_CLAMP_RPS` claimed to cap
   "zero or absurd" readings but capped only zero. A fixture probe published
   about 200,000 items/s.
4. **`RECEIPT_WAIT_MAX` = `HYBRID_IO_TIMEOUT` + 2 x
   `WRITE_QUEUE_DRAIN_BUDGET` = 34 s.** A picked-up write's I/O runs under
   one 30 s deadline and nothing after it awaits, so a wait of at least that
   always ends on a write at the head of its lane. `pending` from a wait now
   means "queued behind other writes", which is the honest answer. This is
   not adaptive: the wait already returns at settle, so a long maximum costs
   only on slow writes. The two bounds the J3 coupled residual needs still
   hold: `MAX_CONCURRENT_RECEIPT_WAITS` caps the population that lengthens
   the proxy's in-flight burst, and serve drops in-flight calls after
   `SHUTDOWN_GRACE`, so a wait never extends a shutdown. The published
   `wait_ms` maximum moves from 4000 to 34000. Raising a maximum accepts
   every previously valid request.
5. **Telemetry for acceptance item 3**: `write_queue_probe_optimism` and
   `write_queue_apply_samples` / `_apply_ms_p50/p90/max` (admission to
   settle over the last 256 applied writes). Additive keys. In the branch's
   burst run they read p50 1,619 ms, p90 13,067 ms, max 14,962 ms: 40
   writes queued on one lane, all inside the new 34 s wait.
6. **#16 §3**: `flush_lag` is the time since the store was last caught up.
   A poll that drains nothing and holds nothing marks it, as a successful
   flush does, except in a degraded session. Idle reads under 100 ms; under
   a failing store it grows exactly as before. Chosen over `n/a` or `null`
   when `log_depth` is 0 because the key is read as a number by every
   consumer, including the session_stats row a reader process reads, and the
   number now means what its doc said.

## For #32 PR 3 (`EmbedderCalibration`)

The probe is now its own type, `writeq::calibration::EmbedderProbe`
(`spawn`, `current`, `abort`), and `WritePipeline` holds one. PR 3 should:

- hold one `EmbedderProbe` per shared embedder process-wide (keyed the way
  the embedder is shared) and give every pipeline an `Arc` to it, instead of
  `WritePipeline::spawn` spawning its own;
- move `abort` from the pipeline's close and `Drop` (`abort_probe`) to the
  owner of the shared probe, since one session closing must not abort a
  probe other sessions read;
- keep the observed rate (`ObservedRate`) and the apply-latency window per
  pipeline, because they measure that pipeline's own writes;
- re-label the probe's log lines, which name the spawning session today.

## Not measured here

- The CUDA rig (#11 acceptance item 1, and item 2 "on both rigs"). Only the
  Metal rig was measured, on a scratch store with a synthetic workload drawn
  to the rig's concept-length distribution, not the live ledger.
- `postgres-live` and `cockroach-live` CI rows need a live database.
