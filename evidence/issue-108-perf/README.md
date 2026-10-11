# 0.4.0 performance against earlier builds (#108)

Two rigs, same method, same conclusion: on a dogfood-sized store, 0.4.0 answers
`lambo_recall` one to two orders of magnitude faster than the build it replaced. It
does so with the same top-8 results, and its writes reach the store sooner.

| File | What it is |
|---|---|
| [`cuda-0.4.0-vs-bef53e6.md`](cuda-0.4.0-vs-bef53e6.md) | The CUDA rig's report, copied from the body of #108. |
| [`ab-metal-0.3.0-vs-0.4.0.json`](ab-metal-0.3.0-vs-0.4.0.json) | `mcp_ab.py ab` output from the Metal rig. |
| [`ledger-metal-0.3.0-vs-0.4.0.json`](ledger-metal-0.3.0-vs-0.4.0.json) | `mcp_ab.py ledger` output from the Metal rig's live call ledger. |

Every JSON file holds aggregates only, with no memory content.

## The two rigs

| | CUDA rig (#108) | Metal rig (this directory) |
|---|---|---|
| Old build | `bef53e6` (Sep 1 branch build) | `v0.3.0`, the previous dogfood pin |
| New build | `v0.4.0` (136209d5) | `v0.4.0` release asset, `macos-arm64-metal` |
| Embedder | BGE-M3, candle on CUDA, dim 1024 | BGE-M3, candle on Metal, dim 1024 |
| Store | 8,496 concepts, 20,526 edges | 4,949 concepts, 12,071 edges |
| Machine | Linux, NVIDIA GPU | Apple M3 Pro, 18 GB |

## Results, client-observed (ms)

| Path | CUDA old p50 | CUDA new p50 | Metal old p50 | Metal new p50 | Metal old p90 | Metal new p90 |
|---|---:|---:|---:|---:|---:|---:|
| `lambo_recall` (top_k 8, n=200) | 804.8 | 15.4 | 310.0 | **5.7** | 332.7 | 12.2 |
| `lambo_derive` ack (n=30) | 1,070.8 | 1.16 | 0.54 | 0.68 | 0.62 | 0.74 |
| `lambo_derive` ack + applied (n=30) | 2,933.5 | 165.3 | 317.8 | **35.9** | 339.3 | 43.3 |
| `lambo_stats` (n=10) | 1.20 | 1.00 | 0.57 | 0.50 | 0.69 | 0.57 |

What the Metal run shows:

- **Recall** is about **55x faster** at p50. Its p99 is 32 ms, against 378 ms on 0.3.0.
- **Result parity.** The top-8 node-id Jaccard between the two builds was 1.0 on all 20 queries. The 0.4.0 ranking changes (#79 cold mode, #95 recency floor) did not move any of these results.
- **Errors**: 0 on both sides.
- **Derive ack** was already non-blocking in 0.3.0, so it is flat here. On the CUDA rig `bef53e6` predates that change, which is why its ack drops by about 900x there.
- **Derive ack + applied**, the time until a write is in the store, went from 318 ms to 36 ms (about 9x).
- **Memory**: RSS after the workload was 252 MB (0.3.0) and 149 MB (0.4.0). On Apple silicon the model weights live in Metal buffers, so RSS undercounts both processes, and the two figures are not comparable with the CUDA rig's.

### Repeated queries vs unseen queries

`mcp_ab.py` repeats 20 fixed queries over 10 rounds. 0.4.0 serves a query it has seen
before in about 5 ms, so the 5.7 ms p50 above is mostly the repeat path. A
separate stdio run asked 75 distinct real queries from the live ledger, each once.
Both builds ran on copies of the same store, after a five-query warm-up:

| | 0.3.0 | 0.4.0 |
|---|---:|---:|
| recall p50 | 315 ms | 26 ms |
| recall p90 | 359 ms | 29 ms |
| recall p99 | 486 ms | 37 ms |
| first recall after start | 360–600 ms | ~200 ms |
| server ready (stdio, model load included) | ~4 s | ~4 s |

So an unseen query is about **12x** faster, and a repeated one about 60x.

## Live ledger cross-check (Metal rig)

`mcp_ab.py ledger --switch 2026-10-11T03:15:44Z --days 3` splits the live writer's
`calls.jsonl` at the re-pin. The three days before the switch were all served by 0.3.0:

| Tool | n | p50 | p90 | p99 | max |
|---|---:|---:|---:|---:|---:|
| `lambo_recall` | 303 | 778 ms | 1,320 ms | 5,714 ms | 13,435 ms |
| `lambo_derive` (server-side) | 290 | 2.1 ms | 3.7 ms | 21.9 ms | 56.1 ms |
| `lambo_record_action` (server-side) | 140 | 3.4 ms | 7.6 ms | 75.6 ms | 78.9 ms |

Live 0.3.0 recall was about 2.5x slower than the isolated A/B. The live writer serves
several agents at once, and the A/B is sequential. 0.4.0 had answered only one live
recall at capture time (111 ms, a first call), so re-run `ledger` mode after a few
days of use for the live-side comparison.

The two builds report `flush_lag_ms` with different meanings, so the ledger's
`flush_lag_ms` block is not a like-for-like comparison and is left uninterpreted.

## Method (Metal)

- Each build got its own copy of `backup-pre-0.4.0-20261011-084541.db`, taken with
  `sqlite3 .backup`. The 0.4.0 copy had `lambo provision` run first. The live writer on
  port 7700 was not touched.
- Each build ran as its own HTTP serve (`:7701` for 0.3.0, `:7702` for 0.4.0) with a fresh
  `XDG_RUNTIME_DIR`. The live writer was resident on the same GPU and idle.
- Command:

  ```
  python3 scripts/loadtest/mcp_ab.py ab --a 0.3.0=http://127.0.0.1:7701/mcp \
      --b 0.4.0=http://127.0.0.1:7702/mcp --rounds 10 --derives 30 --out ab.json
  ```

- Single machine, single run, captured 2026-10-11.

## Follow-up from the CUDA report

0.4.0 does not build with `embed-candle-cuda` against CUDA 13.4. The bundled
`cudarc 0.19.9` knows versions only up to 13.3. The workaround is
`CUDARC_CUDA_VERSION=13030`. A `cudarc` bump or a docs note is still open.
