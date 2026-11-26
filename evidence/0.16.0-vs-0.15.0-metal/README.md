# Lambo 0.16.0 vs 0.15.0 on Metal

Both releases applied all 330 replay calls with identical outcomes. The
first eight recall hits matched by ID, rank, and score on all 200 query
pairs. Sequential recall p50 was 2.17 ms on 0.15.0 and 2.19 ms on 0.16.0.
All 22,388 concurrent writes applied. The canonization dry run took
30–31% less time with 0.16.0, with matching policy reports.

## Test setup

The comparison ran on November 27, 2026, on the M3 Pro dogfood rig with
18 GiB of memory and macOS 26.7.3. Both releases used Candle BGE-M3 on
Metal. The live 0.15.0 writer continued to share the GPU.

- The test used the published `macos-arm64-metal` assets for 0.15.0
  (`8f5fdd07`) and 0.16.0 (`28efff5c`). Both passed SHA-256 verification.
  [0.15.0 release](https://github.com/nrynss/lambo/releases/tag/v0.15.0) ·
  [0.16.0 release](https://github.com/nrynss/lambo/releases/tag/v0.16.0)
- One integrity-checked SQLite backup contained 8,251 concepts across
  eight sessions. All seven saved intent rows had settled.
- The HTTP tests used the `lambo` session with 2,907 concepts, 7,886 edges,
  and 1,158 interactions. The `general` layer contained 41 concepts.
- Each scratch server used a fresh copy, provisioned with its own release
  binary, and a private runtime directory. Both used loopback ports.

An initial HTTP attempt reused one scratch database path. One serve
returned HTTP 503 before measurement. The reported HTTP runs used
independently verified database paths from the same pre-test backup.

## Write replay

`scripts/loadtest/replay_parity.py` reconstructed 300 historical write
calls (42 derives and 258 actions) and added 30 reworded variants. It ran
both sides over stdio with canonization scheduled beyond the run.

Both releases applied 330 of 330 calls. Each created 534 concepts and
semantically merged 26. The final stores both contained 6,157 embedded
concepts and 15,467 edges, with matching counts by edge type and
canonization status. Created concepts and semantic edges matched by
content. The harness reported 45 reconstruction warnings.

## Sequential calls

`scripts/loadtest/mcp_ab.py ab` alternated 20 queries per round across two
HTTP servers for 10 rounds. It then timed 30 synthetic derives per side
through acknowledgement and a receipt wait. Both servers had a 90-second
startup wait, followed by the harness warm-up, and HTTP rate limits were
disabled for this test.

| Client measurement | 0.15.0 | 0.16.0 |
| --- | ---: | ---: |
| Recall p50 / p99 | 2.17 / 25.30 ms | 2.19 / 26.27 ms |
| Stats p50 | 0.39 ms | 0.36 ms |
| Derive acknowledgement p50 / p90 | 0.52 / 0.55 ms | 0.51 / 0.53 ms |
| Derive to applied receipt p50 / p90 | 25.94 / 26.33 ms | 25.99 / 26.16 ms |

Both sides completed without tool errors. The first-round recall set
overlap averaged 1.000 by Jaccard similarity. The server ledgers confirmed
identical ordering and scores for up to eight hits in every measured pair.

## Concurrent agents

`scripts/loadtest/mcp_concurrent.py` ran for 120 seconds per server with
seed 1, the default tool mix, and the default server rate limit. Mean think
time was 1.0 second at eight agents and 0.5 seconds at 16 agents.
The harness waited for the write queue to drain and checked every
acknowledged receipt.

Each release ran once at eight agents and three times at 16 agents with
one scratch server active. The single-server comparisons used three
blocks with alternating release order. Two more 16-agent pairs ran both
servers together, with startup order and ports reversed between pairs.
macOS sleep interrupted several attempts, which were rerun. All reported
runs completed the timed load and checked their receipts before expiry.

| Eight agents, client measurement | 0.15.0 | 0.16.0 |
| --- | ---: | ---: |
| Writes failed / attempted | 0 / 543 | 0 / 543 |
| Recall p50 / p99 | 11.13 / 38.46 ms | 10.45 / 39.21 ms |

All 12 load runs drained. Every acknowledged write reached `applied`, and
no calls failed. The 16-agent tests accounted for 21,302 of the 22,388
writes.

The server-side summaries use the same definitions as the 0.15.0 Metal
gate. Recall latency excludes the first five seconds. A slow call is a
recall or write acknowledgement of at least 100 ms after that window.
Apply lag runs from the acknowledgement to the completion in the serve
ledger. [Previous Metal gate](../0.15.0-vs-0.14.0-metal/README.md)

| Single-server block | Release | Recall p50 / p99 | Apply lag p50 | Writes applied |
| --- | --- | ---: | ---: | ---: |
| 1 | 0.15.0 | 5.13 / 29.47 ms | 52.52 ms | 2,122 |
| 1 | 0.16.0 | 4.90 / 22.98 ms | 51.16 ms | 2,126 |
| 2 | 0.16.0 | 4.94 / 21.59 ms | 51.80 ms | 2,128 |
| 2 | 0.15.0 | 5.06 / 21.49 ms | 46.36 ms | 2,141 |
| 3 | 0.15.0 | 4.95 / 21.63 ms | 50.58 ms | 2,127 |
| 3 | 0.16.0 | 5.07 / 20.70 ms | 46.91 ms | 2,142 |

Single-server recall p50 stayed between 4.90 and 5.13 ms. The p99 ranges
overlapped, and no run had a slow call after the warm-up window.

| Concurrent pair | Recall p99, 0.15.0 / 0.16.0 | Apply lag p50, 0.15.0 / 0.16.0 | Slow calls, 0.15.0 / 0.16.0 |
| --- | ---: | ---: | ---: |
| 0.15.0 started first | 24.31 / 24.16 ms | 93.96 / 93.99 ms | 0 / 0 |
| 0.16.0 started first | 21.99 / 22.14 ms | 89.47 / 86.64 ms | 0 / 0 |

Server-side recall p99 differed by 0.15 ms in each concurrent pair, in
opposite directions. The paired results apply to simultaneous load on
the shared GPU.

## Offline commands

Hyperfine timed `lambo stats --session lambo` 30 times per release and
`lambo canon dry-run` eight times per release, then repeated both batches
in reverse release order. Both commands ran against copies of the same
backup. The dry run used a fixed clock one second after the backup.

| Command | Release order | Mean, 0.15.0 | Mean, 0.16.0 |
| --- | --- | ---: | ---: |
| Session load | 0.15.0 first | 157.19 ms | 153.54 ms |
| Session load | 0.16.0 first | 156.95 ms | 160.12 ms |
| Canonization dry run | 0.15.0 first | 2.441 s | 1.686 s |
| Canonization dry run | 0.16.0 first | 2.407 s | 1.687 s |

Session load varied by about 2% in opposite directions. The canonization
dry run took 30–31% less time on 0.16.0 in both orders. Both releases
produced identical Swarm and Solo policy reports, including 30 and 34
cycles to a fixed point respectively. All invariants held and both
commands left the scratch database unchanged.

## Scope and files

This comparison measures standard operation on SQLite and Metal. The
snapshot contained no owed intents. The tests for rejected startup replay,
PostgreSQL provisioning, and Elasticsearch mirror repair are in their
respective fix PRs. [Replay #354](https://github.com/nrynss/lambo/pull/354) ·
[PostgreSQL #360](https://github.com/nrynss/lambo/pull/360) ·
[Elasticsearch #361](https://github.com/nrynss/lambo/pull/361)

[Aggregate results](results.json) include write totals, client-side latency
summaries, offline timing samples, release commits, and asset checksums.
The HTTP harnesses export client-side quantiles as summaries.

[HTTP samples](http-samples.json) contain the underlying server-side
measurements and all 200 ranked comparisons. Each hit has a report-local
integer that lets you match it between releases. The capture omits query
text, memory text, private paths, and store identifiers.

Each load run contains batches of numeric call rows and batches of apply
lags. Flatten each set of batches before calculating quantiles. Divide
microseconds by 1,000 to report milliseconds. Use sorted index
`round(p * (n - 1))` for percentile `p` and sample count `n`. For recall,
filter out rows ending within five seconds of the first tool call.

## Reproduce the measurements

Run the commands from the repository root with the release binaries and
scratch paths in the variables shown. Remove inherited `LAMBO_*`
environment overrides. Keep the snapshot, configs, and ledgers outside
the checkout.

Replay uses the SQLite and Metal config template. Use separate output and
work directories for a reconstruction-only pass with `--dry-run`.

```bash
python3 scripts/loadtest/replay_parity.py \
  --bin-a "$BIN_15" --bin-b "$BIN_16" \
  --snapshot "$SNAPSHOT" --config-template "$CONFIG_TEMPLATE" \
  --session lambo --groups 300 --variants 30 \
  --canon-eval-secs 100000000 --canon-wait 0 \
  --out "$REPLAY_RESULTS" --work "$PRIVATE_REPLAY"
```

Before each HTTP run, copy the database and set the scratch config to that
copy. Use `candle`, `device = "metal"`, and `dim = 1024`. Serve
`lambo` with the `general` layer at weight and minimum similarity 0.5.
Use a separate port and runtime directory for each writer.

```bash
"$BIN" --config "$CONFIG" provision
XDG_RUNTIME_DIR="$RUNTIME_DIR" "$BIN" --config "$CONFIG" serve \
  --transport http --bind 127.0.0.1 --port "$PORT" --ledger "$LEDGER"
```

Add `--rate-limit-rps 0` for both sequential A/B servers. Wait 90 seconds
after both ports open and use their loopback `/mcp` URLs.

```bash
python3 scripts/loadtest/mcp_ab.py ab \
  --a "0.15.0=$URL_15" --b "0.16.0=$URL_16" \
  --rounds 10 --derives 30 --out "$AB_RESULTS"
```

Keep the default rate limit for concurrent runs. Wait 15 seconds after
the serving port opens, or after both ports open for a concurrent pair.
Set `WORKERS=8` with `THINK=1.0`, or `WORKERS=16` with `THINK=0.5`.

```bash
python3 scripts/loadtest/mcp_concurrent.py \
  --endpoint "$URL" --out "$LOAD_RESULTS" --workers "$WORKERS" \
  --think "$THINK" --secs 120 --seed 1 --receipt-max 5000 \
  --server-ledger "$LEDGER"
```

Set `CANON_CLOCK` to `2026-11-26T20:39:21Z` to reproduce the clock used
with this backup. Swap the command order for the second timing batch.

```bash
hyperfine --shell=none --warmup 2 --runs 30 --export-json "$STATS_TIMINGS" \
  --command-name 0.15.0 "$BIN_15 --config $CONFIG stats --session lambo" \
  --command-name 0.16.0 "$BIN_16 --config $CONFIG stats --session lambo"

hyperfine --shell=none --warmup 1 --runs 8 --export-json "$CANON_TIMINGS" \
  --command-name 0.15.0 "$BIN_15 canon dry-run --store $STORE --session lambo --at $CANON_CLOCK --json" \
  --command-name 0.16.0 "$BIN_16 canon dry-run --store $STORE --session lambo --at $CANON_CLOCK --json"
```
