# 0.8.0 and 0.12.0: the re-pin gate

The checks the dogfood rig runs before a re-pin, run for a jump of four
minor versions this time: the rig went from 0.8.0 straight to 0.12.0,
skipping 0.9.0, 0.9.1, 0.9.2, 0.10.0, 0.10.1 and 0.11.0 (73 commits, tag
`v0.8.0` 15585529 to tag `v0.12.0` 015d4acc). Same gate as
[0.6.0 vs 0.5.0](../0.6.0-vs-0.5.0/README.md) and
[0.6.0/0.7.0 vs 0.8.0](../0.8.0-vs-0.6.0/README.md): an interleaved A/B of
the two builds on copies of the live store, plus the static review of what
the span changes, plus the 0.12.0-specific names preview.

- **When:** 2026-11-17.
- **Builds:** 0.8.0 is the rig's pinned binary (tag `v0.8.0`, 15585529).
  0.12.0 was built at a784564e, the `main` tip of that day, which is
  code-identical to tag `v0.12.0` (015d4acc); the two commits between them
  touch documentation only. Both built with
  `--features store-sqlite,embed-candle-cuda,embed-bge` and
  `LAMBO_GIT_SHA` set. **Build note:** this build needs
  `NVCC_CCBIN=clang`; the system GCC (16.2.1) is too new for CUDA 13.5's
  nvcc, which fails candle-kernels with `Failed to preprocess host
  compiler properties`. cudaforge reads `NVCC_CCBIN` as nvcc's `-ccbin`.
- **Machine:** Linux x86_64, RTX 4070 SUPER (12 GB), CUDA 13.5. Every
  build embeds with candle BGE-M3 on CUDA.
- **Store:** each side served its own copy of the dogfood store, taken
  with SQLite's backup facility while the writer was live (11,218
  concepts at copy time). The live writer stayed up on the same GPU for
  the whole run and was not idle — other agents kept writing — which is
  what the A/B's interleaving is for: drift and contention hit both sides
  equally.
- **Provisioning:** the 0.12.0 side's copy was provisioned by the 0.12.0
  binary before serving. It adds the `concept_names` and
  `retired_aliases` tables and nothing else: no vector transcode, no
  re-key (`canonical_key_rule` has been in `store_meta` since the 0.6.0
  gate's #148 re-key).
- **Method:** `scripts/loadtest/mcp_ab.py ab`, 10 rounds over its 20
  recall queries per side (interleaved A/B per query), 30 synthetic
  derives per side with receipt round-trips, sequential calls — latency,
  not throughput. Ports 7701/7702; the tool refuses the production port.

These files hold aggregates only, with no memory content and no local
paths. `ab.json` is the tool's output; `names-preview.json` is the
`lambo names migrate` preview run on the provisioned copy (preview holds
no lease and writes nothing).

## Summary

| Test | 0.8.0 | 0.12.0 | Verdict |
|---|---:|---:|---|
| recall latency p50 / p99 (ms) | 12.09 / 57.96 | 12.32 / 30.34 | identical median; the 0.12.0 tail is better |
| `lambo_stats` p50 (ms) | 1.38 | 1.31 | identical |
| derive ack p50 / p90 (ms) | 28.57 / 55.99 | 5.58 / 13.88 | **~5x faster** |
| derive applied (ack to applied receipt) p50 / p90 (ms) | 73.67 / 108.31 | 44.74 / 63.96 | **~40% faster** |
| errors (recall + stats + derives, both sides) | 0 | 0 | no failures |
| recall top-k Jaccard, 20 queries | — | — | **1.0 on every query: identical recall output** |

**No regressions were found, static or measured.** Recall output is
byte-for-byte the same hits on every query, which is the expected result
made empirical: every ranking-constant change in the span (Qwen
calibration and RRF fusion, the Elastic `.jina-embeddings-v3` row, EG2)
touches contracts this rig does not use, and BGE-M3 is the baseline the
others are calibrated toward. The research-175 graph-relevance proposal
was evaluated on the rig's own frozen snapshot, archived as a negative
result, and retired without a production change.

The write path got faster, not just safe: the derive acknowledgement
dropped from 28.6 ms to 5.6 ms p50 and the full ack-to-applied receipt
round-trip from 73.7 ms to 44.7 ms, measured on the real 11k-concept
graph. Nothing in the changelog claims this; it is what the durable
intent and names-admission rework in 0.9.0 through 0.12.0 buys.

## What the span gives this rig

- **Durable concept names (#146), the headline.** Stable, normalized,
  session-local names bound permanently to concept IDs; usable in
  `depends_on`, `against` and inspect focus; identity pinned across
  rollback, shutdown and restart replay. A registered name with a
  missing target fails explicitly instead of creating a phantom concept,
  and a deleted target keeps its name reserved. For this store the
  upgrade is provision-only: the `names migrate` preview on the
  provisioned copy proposed **0 bindings and 0 retirements** (no
  surviving legacy synonyms, no alias-Entity artifacts).
- **Inspect warns where recall warns** (0.9.1 #226, 0.9.2): a
  contradicted focus carries the same warning line recall renders, in
  the CLI, the MCP tool and the web portal.
- **Uncalibrated contracts are no longer silent** (0.9.0 #221): one
  startup log line and a `calibration=` stats line.
- **Dynamic web sessions** (0.11.0 #241): a web credential's explicit
  `session_prefix` reads existing sessions created after `serve-web`
  starts, bounded and never creating sessions. Matters here only once
  web credentials exist.
- **Hosted-embedder hardening** (0.10.0/0.10.1 #233 #236 #237 #238):
  configurable connect/read timeouts, a transport failure named by
  cause, a response cut mid-body classified as an outage whose durable
  intent survives for replay, API keys never crossing plain http and
  never echoed back, oversized responses refused unread. Irrelevant to
  the in-process candle writer, load-bearing for any hosted-embedder
  deployment (the dresscode rig's Workers AI BGE-M3, for one).
- **Release rigor** (0.9.2): releases tag a commit that has passed full
  CI for that exact commit.

## What the span does not change

- **The embedder and protocol stack.** candle, cudarc, cudaforge and
  rmcp versions are unchanged between the two tags, and so is
  `src/ledger.rs`. The canary pin stays valid, no re-embed is needed,
  MCP client surface is stable, and the ledger before/after split stays
  meaningful across the re-pin.
- **Recall ranking under BGE-M3.** All constants and fusion changes
  touch Qwen, Elastic and EmbeddingGemma 2 contracts only.

## Re-pin record (2026-11-17)

- Outage window about 40 seconds: stop the web reader and the writer
  (graceful, lease released), take the pre-upgrade backup, `lambo
  provision` with the 0.12.0 binary, stage `lambo-0.12.0`, re-point the
  web unit, start. All five pinned sessions re-attached with the same
  lease holder; the web portal served HTTP 200 on the new binary;
  `lambo_stats` answered through the new writer with the pre-upgrade
  epoch unchanged.
- The backup directory holds the store copy **and the 0.8.0 binary**
  together, because rollback is restore-the-pair: an older binary
  ignores the new tables, but downgrade is unsupported once any name is
  declared, and the supported path is the pre-upgrade backup.
- Follow-up: run `mcp_ab.py ledger` against the serving ledger at the
  re-pin timestamp once 0.12.0 has served for a while, for the
  server-side before/after latency split.

## Hazards the gate surfaced

1. **Never stage a new binary in the rig's `bin/` before provisioning.**
   `serve-http.sh` execs the newest `lambo-*` there, so a writer
   crash-restart between staging and provisioning boots the new binary
   against the old schema and restart-loops the rig's writer.
2. **The web unit hardcodes the binary path**, unlike the writer's
   wrapper: every re-pin needs its `ExecStart` edited and a
   `daemon-reload`, or the portal silently stays on the old binary.
3. **candle-CUDA builds need `NVCC_CCBIN=clang`** on this box until the
   CUDA toolkit supports the system GCC (see build note above).
4. **Session splitting refuses snapshots that contain durable names or
   retired-alias history.** The multi-session cut-over, if it is still
   planned, must split before any name is declared on a session.
5. **PATH-binary consumers inherit upgrades.** Any other rig serving the
   `~/.local/bin` binary boots the new version at its next restart and
   must provision its store first. Such a rig exists on this machine
   (the dresscode writer); it was flagged to the operator during this
   re-pin.
