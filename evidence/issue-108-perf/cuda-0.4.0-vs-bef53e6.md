# CUDA rig: 0.4.0 vs bef53e6

Copied verbatim from the body of nrynss/lambo#108. The inline script it carried has since been merged as [`scripts/loadtest/mcp_ab.py`](../../scripts/loadtest/mcp_ab.py) (#109), so the copy here is omitted.

## Summary

A/B of the dogfood writer before and after the 0.4.0 re-pin, on identical copies of the dogfood store (11,186 nodes, 20,526 edges, 8,496 concepts, 8,461 embedded, BGE-M3 via candle on CUDA, dim 1024).

- **old**: `lambo-bef53e6` (Sep 1 branch build, store-sqlite + embed-candle-cuda)
- **new**: `v0.4.0` (136209d5, same features, built with `CUDARC_CUDA_VERSION=13030`, see below)

| Path (client-observed, ms) | old p50 | old p90 | new p50 | new p90 | Change (p50) |
|---|---:|---:|---:|---:|---:|
| `lambo_recall` (top_k 8, n=200) | 804.8 | 886.0 | 15.4 | 19.8 | **~52x faster** |
| `lambo_derive` ack (n=30) | 1070.8 | 1132.1 | 1.16 | 1.25 | ~900x faster (ack no longer blocks) |
| `lambo_derive` ack + applied (n=30) | 2933.5 | 3038.6 | 165.3 | 192.8 | ~18x faster |
| `lambo_stats` (n=10) | 1.20 | 1.40 | 1.00 | 1.19 | flat |

- **Recall quality unchanged**: top-8 node-id Jaccard between old and new was 1.0 on all 20 queries.
- **Errors**: 0 on both sides.
- **Memory**: RSS after the workload was 1,559 MB (old) and 1,791 MB (new), about +232 MB. GPU memory was 1,380 MiB per process on both.
- **new p99 recall** was 60 ms (max 128 ms), so there is a small tail. Still well under old p50.

### Cross-check against the live ledger

The old writer's own `calls.jsonl` over the 14 days before the switch agrees with the A/B: `lambo_recall` p50 853 ms, p90 1,058 ms, p99 1,793 ms (n=659, 0 errors). So the ~800 ms recall is real production behaviour on this store, not an artifact of the test rig. The new writer has too few live calls so far for a ledger comparison; re-run `ledger` mode after a few days.

The earlier recall profile on this rig measured ~155-175 ms. That was on a much smaller graph, and the same profile found recall cost scaled with embedded concept count. At ~8.5k concepts the old binary's scan reaches ~800 ms, which 0.4.0 removes.

One discrepancy to note: the old ledger times `lambo_derive` server-side at p50 0.06 ms, while the client saw ~1,070 ms per derive ack in the A/B. The ledger timing in `bef53e6` evidently does not cover the whole request. The A/B numbers above are client-observed round trips and are the ones to trust.

## Method

- Both binaries ran side by side as HTTP serves (`:7701` old, `:7702` new), each on its own copy of the same pre-switch backup. new's copy had `lambo provision` run first, as 0.4.0 requires (adds `concepts.embedding_source` and the persisted GC/epoch columns).
- Workload: 3 warm-up recalls per side, then 10 rounds x 20 fixed topic queries, **interleaved old/new per query** so GPU clock and page-cache drift affect both equally; one `lambo_stats` per round; then 30 synthetic `lambo_derive` writes per side, each followed by `lambo_stats(receipt, wait_ms=4000)` to time until it was applied.
- Calls were sequential (one in flight at a time), so this measures latency, not throughput under concurrency.
- The live dogfood writer was also resident on the same GPU, idle, during the run.

## Caveats

- Single machine, single run. Ratios are large enough that run-to-run noise does not change the conclusion.
- `derive ack + applied` is the ack round trip plus the receipt wait, a client-side approximation of time until the write was applied.
- 0.4.0 does not build with `embed-candle-cuda` against CUDA 13.4: the bundled `cudarc 0.19.9` only knows up to 13.3 and panics in its build script. `CUDARC_CUDA_VERSION=13030` works around it (13.x minor versions are ABI compatible). Worth a `cudarc` bump or a docs note.

