# F4 — close-time flush latency vs the real Cockroach cluster (measurement)

**Verdict: the close-time flush blows `CLOSE_FLUSH_GRACE` at a tiny durable-intent
tail, and past that point it abandons and LOSES the tail.** Measured against the
live CockroachDB serverless cluster (GCP asia-south1, the `.env` DSN), release
binary with the fixture embedder (BGE-M3 at 8080 is down; the flush cost under
measurement is independent of the embedder).

**Machine:** `cachyos-x8664`, AMD Ryzen 5 3600 (6C/12T), CachyOS `7.1.8-1-cachyos`.
**Binary:** `target/release/lambo` (0.2.2, `fe98b23`), built
`cargo build --release --features store-cockroach,embed-fixture`.
**Config:** `lambo.cockroach.toml` in this directory — `[store] kind="cockroach"`
(DSN from `LAMBO_COCKROACH_DSN`, never stored/printed here), `[embedder]
kind="fixture" dim=1024`. Auth token was a scratch value, redacted.

## Method (driver adapted from `scripts/loadtest/capture_sigterm.sh` + `mcp_load.py`)

For each tail size K (durable intents pending at close), a **fresh scratch
session** was provisioned, `lambo serve --transport http` was started against
the live cluster, a burst of K `lambo_record_action` writes was fired as fast as
round-trips allow, and SIGTERM was pulled **the instant** the burst finished.
Close-flush wall time = `winding down` → `session closed, tail durable` (from
the serve stderr timestamps); an abandoned close (the `did not finish within the
grace window` error) is reported as ≥ 8 s with the tail LOST (exit 1).

Driver: `f4_drive.py` (per=16 → one durable intent per record_action — verified
1:1 against `write_intents`), orchestrated by `f4_point.sh` (per point) /
`f4_sweep.sh` (settled variant). Capture files `<run>.log` / `<run>.jsonl` are
the verbatim stderr + driver ledger for each K; the store counts were read back
via the DSN (psql).

The write-behind daemon (1 s cadence, 500-mutation batch cap) drains between the
burst and the close; for the landed points above the mid-range the close drain
was the whole burst, so **K = number of durable intents the close flush had to
carry** is read one-to-one from the acked count (all land) for the durable rows,
and the abandoned rows are those where the close lost the remaining tail.

## Per-K table (K durable intents pending at close)

| K | close-flush ms | exit | outcome | durable intents after close | notes |
|---|---|---|---|---|---|
| 10 | 2718 | 0 | flush completed | 10/10 | 540-mutation close tail |
| 25 | 6449 | 0 | flush completed | 25/25 | 1350-mutation close tail |
| 30 | 7708 | 0 | flush completed (edge) | 30/30 | ~96% of the 8 s budget |
| 35 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 0/35 | every acked write lost |
| 50 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 15/50 | 35 lost |
| 75 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 0/75 | all lost |
| 100 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 0/100 | all lost |
| 150 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 0/150 | all lost |
| 400 | ≥ 8000 (timeout) | 1 | **abandoned — tail LOST** | 22/400 | 378 lost |

Abandoned = `close() did not finish within the grace window ... un-flushed tail
is LOST ... no on-disk WAL`. Settled sweep corroboration (`f4-210749-*`):
K=50 → 4.4 s durable; K=100 → timeout, 32/100 durable; K=200 → timeout,
38/200 durable.

## Fit

Linear least squares over the landed flush windows (K=10: 2718, K=25: 6449,
K=30: 7708 ms):

```
close_flush_ms(K) = 221 + 249.4 · K        (R² ≈ 1.00 over the three landed points)
```

- **Per-durable-intent cost: ≈ 249 ms** (marginal — 3 extra statements per
  intent, `UPDATE` + retention `DELETE` + insert, each on a round trip, plus the
  two `plan_flush` bucket drains per intent, against the ~40 ms RTT serverless
  cluster).
- **Intercept (base close cost): ≈ 221 ms** (shutdown + quiesce + lease release).
- **K crossing `CLOSE_FLUSH_GRACE` (8000 ms): K ≈ 31** — confirmed by the data:
  K=30 lands at 7.7 s, K=35 abandons at the 8 s grace.

This matches the pessimistic barrier model (2 drains + 3 statements per
durable-intent write), not the benign planned-statement model. The multi-row
statement batching L82-1 introduced is fragmented at every durable-intent
barrier, so the per-write cost is several round trips, not a fraction of one.

## Verdict

**F4's worry is confirmed, with margin.** A realistic un-flushed durable-intent
tail of ≈ 31 already consumes the whole 8 s close budget; at ≥ 35 the close
abandons and — because the serve path has no on-disk WAL — **the acked tail is
lost** (verified: the store ends short of every ack, e.g. 35/35 lost at K=35).
No "realistic tails" stay inside `CLOSE_FLUSH_GRACE`: any burst of more than a
handful of writes that outpaces the 1 s write-behind daemon is a data-loss
close. The barriers (`plan_flush`'s `bucket.drain_into` per durable-intent
mutation) are the thing to attack, exactly as the design doc's "if the risk
below materialises, the barriers are what to attack" anticipated.

## Files

- `lambo.cockroach.toml` — exact config (crdb + fixture, DSN from env).
- `f4_drive.py`, `f4_point.sh`, `f4_sweep.sh`, `f4_run.sh` — the drivers.
- `stderr-f4q-211014-{10,25,50,75,100}-*.log`, `stderr-f4q-211122-{30,35,150,400}-*.log`
  — verbatim serve stderr per K (the close lines are the primary evidence).
- `stderr-f4-210749-{25,50,100,200,400}-*.log` — settled-sweep corroboration.
- `ledger-*.jsonl` — driver response ledger per K (acks / receipts / errors).

All scratch sessions and rows left in the cluster were cleaned up; nothing in
this directory contains the DSN or a password.
