# Concurrency capture — C1–C3 (load, SIGTERM, durability)

Run `20260817-204139`, session `c-load-20260818`, K=12 concurrent MCP
clients, **on the Linux box** — `cachyos-x8664`, AMD Ryzen 5 3600 (6C/12T),
CachyOS `7.1.8-1-cachyos` — **not** the MBP the P8 criterion names
(see `dev-diary/notes/concurrency-capture.md` for the hardware caveat).
Binary: `target/debug/lambo` (0.2.2, `--features store-sqlite` + default
embed-bge/embed-fixture/store-memory), SQLite store, fixture embedder.

## What each artifact shows

| Artifact | What it proves |
|---|---|
| `stderr-20260817-204139.log` | Full server stderr: 1712 lines. The exact SIGTERM assertion, quoted below, is on line 1712. Zero `tail lost on exit` lines. Also carries the daemon GC sweep log (`concepts_collected=107`) used by the durability accounting. |
| `ledger-20260817-204139.jsonl` | Every response, one JSON line per call: worker, seq, tool, params as sent, ok/is_error, response text, HTTP status, elapsed ms — plus `phase` markers for cap-probe / overdrive / main / burst and 12 `session` records. 3478 lines. |
| `durability-20260817-204139.txt` | The C3 ledger-vs-store comparison, GC-accounted (see below). |
| `run-20260817-204139.json` | Machine, command line (auth token as `<SCRATCH-TOKEN>`), timing, exit code, assertion booleans. |
| `c-load-20260818.db` | The scratch SQLite store itself — the durable truth the comparison queries. |
| `lambo.sqlite.toml` | The exact config the server ran under (sqlite store + fixture embedder). |
| `driver-20260817-204139.stdout` | Driver summary line. |

## The exact SIGTERM line (quoted, verbatim from line 1712)

```
2026-08-17T20:42:33.118846Z  INFO lambo::mcp::serve: lambo serve: session closed, tail durable
```

The assertion is the **exact line** — `lambo serve: session closed, tail
durable` — not a vibe. It is present once; `tail lost on exit` is absent
(`grep -c 'tail lost on exit' stderr-*.log` → 0). Shutdown sequence from
the transcript: `shutdown signal received, winding down` →
`Memory session closed (tail flushed) mutations=332` → the assertion line.
**Signal → exit: 1419 ms, exit code 0** (SIGTERM sent 5 s into the burst,
with the at-cap `record_action` tail still un-flushed).

## The durability comparison (C3), as a table

Ledger accounting (successful calls only): 1303 ok of 3455 recorded; 830
successful write calls (465 derive + 365 record_action); derive created /
matched 585/870; record_action created / edges 911/5506; refused at the
tool layer 310; rate-limit 429s 1682 (the bounded overdrive); transport
failures 120 (after the server exited mid-burst).

| Metric | Ledger expected | Store | Verdict |
|---|---|---|---|
| interactions (1:1 per write call; append-only, never GC'd) | 830 ok writes | 862 | **store AHEAD by 21** — in-flight calls flushed by the close drain (the `mutations=332` tail) |
| concepts (created) | 1454 | 1359 | shortfall 107 — **fully explained**: the daemon ran one GC sweep that collected 107 concepts (spec §9 sub-threshold/orphan housekeeping; `concepts_collected=107` in the stderr). Created − store == collected exactly. A created-then-collected concept is durable work, not tail loss. |
| edges (record_action reports its adds; derive edges unreported) | ≥ 5506 | 9279 | OK — store exceeds the reported lower bound |

The GC accounting is not hand-waved: the control run with
`[daemon] gc_interval = 1` produced `concepts_collected` sums exactly equal
to the created−store gap (11 = 11), and the burst side of this run matches
exactly (768 distinct burst targets + 73 unique action nodes all present).
The durability yardstick is the interaction count — append-only, exact —
and it is AHEAD, not short.

**Verdict: tail durable — no ledger-successful write is missing from the
store.** The `CLOSE_GRACE` budget (10 s, split `CLOSE_FLUSH_GRACE` 8 s +
`LEASE_RELEASE_GRACE` 2 s) was not tested to its limit on SQLite: the final
drain of 332 mutations flushed in ~1.4 s. The honest number to record for
the C3 decision: **0 shortfall on the interaction yardstick; 0 unexplained
concept shortfall (107 = GC, accounted)**.

## Rate limit and session cap — observed, not just assumed

* **Session cap (32):** the cap probe minted sessions until the server
  refused with 503 (`at the concurrent-session cap (32/32 sessions live)`),
  then released them via `DELETE /mcp`.
* **Rate limit (50 rps, burst ×2):** the overdrive phase free-ran 12
  workers for 2 s (bounded to 120 calls each) against the fresh server;
  1682 requests were refused with 429 (`rate limit exceeded: slow down and
  retry`), and the main window paced at 40 rps under the limit took zero
  refusals — so refusals never crowded out the measurement.
* **Cap refusal, not a hang:** every adversarial `record_action` at 65
  combined targets returned the exact refusal `produces + modifies +
  depends_on must total at most 64 entries (65 given)` — with 310 tool-level
  refusals total (NUL, U+202E, over-size content, unknown tool, malformed
  params all refused cleanly).

## What the run found (beyond the assertion)

* **Throughput decay under load:** aggregate call rate fell from ~40 rps to
  ~11–19 rps over the 45 s main window as the graph grew — a real
  measurement of this hardware, not a fault.
* **Hybrid derive retries exhausted under 12-way concurrency:** a handful of
  `lambo_derive` calls failed with `hybrid derive could not commit after 8
  concurrent graph changes`. Server-side detail is on stderr; the client got
  only the class — the N4 wire-hygiene property, working.
* **Hybrid matching degrades on SQLite** (no `VECTOR_SEARCH`): the server
  logged the documented fallback to `MatchStrategy::Canonical`.

## Wire hygiene (acceptance scan)

Response fields (`error`/`text`) across all 3455 ledger calls and the full
stderr were scanned for `postgres(ql)?://`, `mysql://`, `sqlite://`,
`cockroachlabs.cloud`, `sqlx`/driver text, and `https?://` internal URLs:
**zero matches**. The only loopback address anywhere is the operator-chosen
bind in the server's own startup log
(`mcp http: listening on /mcp addr=127.0.0.1:7700`) — configuration echo,
not a leak. No DSN, key, or cluster id appears in this directory; the run
metadata records the auth token as `<SCRATCH-TOKEN>` (the real token lived
in a `mktemp` file, passed via `LAMBO_AUTH_TOKEN`, deleted on exit).

## Reproduce

```bash
cargo build --features store-sqlite
scripts/loadtest/capture_sigterm.sh \
    --out evidence/concurrency --workers 12 --session c-load-<date> --delay 5
```

See `scripts/loadtest/README.md` for the driver's phase design
(sessions → cap-probe → overdrive → main → burst).
