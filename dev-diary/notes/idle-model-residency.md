# Idle model residency: embedder keep-warm for `lambo serve` (issue #13)

**Decision (2026-10-07):** `lambo serve` runs a holder-side keep-warm task that
embeds one short fixed probe (`"lambo keep-warm probe"`) every interval and
discards the vector. Knob: `[embedder] keep_warm_secs` /
`LAMBO_EMBED_KEEP_WARM_SECS`. Omitted = auto (30 s for candle on Metal, off
elsewhere), `0` = off, `N` = every N s for any kind. Code:
`src/embed/keep_warm.rs`, wired in `src/mcp/serve.rs` beside the ledger
heartbeat, resolved by `ResolvedBackends::keep_warm_interval()`.

## Diagnosis

The issue's table (live Metal rig, `lambo-e11fb06`, candle on Metal): recall p50
186 ms at < 2 min since the previous call, 780 ms at > 30 min (p90 9 s), against
71 ms for an isolated probe on the same store. The audit on the issue
established that nothing in the process touches the model weights between
calls: the ledger heartbeat reads graph counters, the daemon tick and flush
loop touch only the graph, GC is mutation-gated.

What this work adds:

1. **Reproduced read-only on the writer today** (pid 2011, up 21 h): footprint
   1518 MB, `CMPRS` 1357 MB, RSS ~172 MB; system swap 8.2 of 9.2 GB used. The
   process shape is the one the issue reports.
2. **`CMPRS` is not a valid confirmation metric.** A fresh probe process
   (same adapter, same weights, Metal) reports ~1.12 GB compressed immediately
   after loading and 24 back-to-back embeds at 20-22 ms each, and ~1.37 GB in
   both arms of the A/B below, keep-warm on or off. A large `CMPRS` coexists
   with warm latency, so the issue's acceptance item "CMPRS staying small
   while idle" cannot confirm or refute the mechanism. Latency by gap can.
3. **Two costs, not one.** After only a 5 s gap the first embed is 47-55 ms
   against a 20 ms second call, in both arms. The pager does not act on a 5 s
   timescale; this is a wake/ramp cost (GPU/CPU power state, command-queue
   cold) that no keep-warm interval removes. The cost that keep-warm does
   remove grows between 5 s and 60 s of idleness (see Measurement). Which
   kernel mechanism carries it (compression, Metal residency eviction, GPU
   power gating) is not pinned; the fix does not depend on which.
4. **The launchd job has no `ProcessType`.** `man launchd.plist`: if
   unspecified, "the system will apply light resource limits to the job,
   throttling its CPU usage and I/O bandwidth". The isolated 71 ms probe ran
   from a terminal, unthrottled. That is a candidate for the gap-independent
   part of the tax (back-to-back live calls at 2.5x the probe) and is an
   operator change (`ProcessType = Interactive` or `Adaptive` in
   `dev.lambo.dogfood.plist`), not code. Not measured.

## Measurement

A/B on this Mac (M3 Pro 18 GB, macOS, the same machine as the live rig;
system swap 8.2/9.2 GB used at the time), 2026-10-07. Two separate processes,
run concurrently so they share memory-pressure conditions, each loading the
candle adapter on Metal from the cached published f16 weights (offline) and
timing a realistic 14-word query embed after each idle gap, then once more
back-to-back. One arm runs `keep_warm_loop` at 30 s, the other none. A
throwaway example binary, not committed. **n = 1 per gap per arm**, so read
the shape, not the decimals.

| idle gap | keep-warm off: first / second | keep-warm 30 s: first / second |
|---|---|---|
| warm, back-to-back (p50 of 12) | 21.9 ms | 21.5 ms |
| 5 s | 54.6 / 20.1 ms | 46.5 / 20.0 ms |
| 60 s | **175.0** / 21.0 ms | **55.3** / 19.9 ms |
| 180 s | **206.4** / 20.1 ms | **58.7** / 20.5 ms |
| 420 s | **149.4** / 19.8 ms | **53.7** / 19.8 ms |

Probe text forward (the touch itself): p50 17.3 ms warm.
`top` CMPRS was 1.12-1.38 GB in **both** arms at every sample.

Reading: keep-warm removes the gap-growing component (150-206 ms down to
54-59 ms at 60-420 s, which is about the 5 s-gap figure); what remains is the
~30 ms wake cost a 5 s gap already shows, which an interval cannot reach. The
off arm's 60 s figure (175 ms) is close to the live rig's < 2 min bucket
(186 ms p50).

Not measured: gaps beyond 7 minutes, the > 30 min bucket and its 9 s p90
(a fresh process does not age the way a 21 h writer under 8 GB of swap does),
and anything on the live writer itself. The store and graph side of a real
recall was not in this probe (embed only).

## Design

* **What a touch does:** one minimal forward through the same embedder
  instance the session uses (`Memory::embedder()`). It writes nothing: no store
  I/O, no graph mutation, no ledger line, no recall-cache entry. It is not part
  of the embedding contract.
* **Where:** spawned on the holder path below the shutdown arming, beside the
  heartbeat, before the serve-level "session attached" line. Spawning awaits
  nothing, so the pre-handshake window is unchanged; the first touch is one
  full interval after arming, so startup gains no forward. Aborted at close
  with the heartbeat. Proxies (no embedder) and one-shot CLI commands never
  run it. Off for fixture builds, so `serve_pre_handshake_durability` runs the
  same timeline as before.
* **Cadence:** sleep after each touch, not an interval timer, so a stalled
  touch (cold swap-in, slow remote) is followed by a full interval of quiet,
  never a catch-up burst. A failing embedder warns once per outage.
* **Auto rule:** on only where the weights sit in pageable unified memory in
  this process, i.e. candle on a Metal device (`weights_in_unified_memory`, a
  downcast like `candle_identity`). CUDA weights are in VRAM and the CUDA rig
  shows no tail; the fixture has no weights; `bge_m3` and `gemini` hold their
  weights in another process. An explicit `N` opts any kind in (a `bge_m3`
  llama-server on the same Mac, a CPU-pinned candle).
* **30 s default:** the rig already paid 2.6x at its shortest bucket
  (< 2 min), so the period must sit well inside that; 30 s is a 4x margin. A
  probe forward costs ~17 ms on the M3 Pro, so ~0.06% GPU duty.
* **Config path:** `EmbedderConfig.keep_warm_secs` (TOML, `deny_unknown_fields`
  still refuses typos), env overlay in `overlay_env`, name added to
  `RESOLVE_ENV_VARS` and its override table. Derived by a method on
  `ResolvedBackends` rather than a new field, so the single resolve site gains
  no field to keep in sync.

## Alternatives rejected

* **Ride the ledger heartbeat.** A rig may run without `--ledger`, and 300 s
  is far outside the window where the tax already shows.
* **Also touch the store (a vector-candidates read).** SQLite file pages are
  file-backed (evicted and re-read, not compressed); the graph is already
  touched every second; the scan costs ~64 us per concept (#8) and on
  Postgres/Cockroach would be periodic network traffic. Revisit only if the
  rig measurement leaves a store-shaped residue.
* **`mlock` the weights.** Competes with the rest of an 18 GB laptop, and the
  Metal buffers are candle's allocations, not ours to wire.
* **Skip touches when recently active.** Needs activity tracking at every
  embed call site to save one minimal forward per 30 s.
* **Keep-warm in `lambo serve-web`.** Out of scope; it is a reader with its
  own lifecycle (#28 is decomposing it).

## Not done / follow-ups

* Live-rig confirmation against the issue's acceptance (p50 at > 30 min within
  2x of the isolated probe; p95 < 1 s over a week) needs the rig re-pinned to
  a binary carrying this change. Not done here: the live writer was not
  touched.
* The `ProcessType` A/B on the launchd job (operator change).
* Replace the issue's `CMPRS` acceptance item with a latency-by-gap one.
