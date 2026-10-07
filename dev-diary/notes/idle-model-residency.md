# Idle model residency: embedder keep-warm for `lambo serve` (issue #13)

**Decision (2026-10-07):** `lambo serve` runs a holder-side keep-warm task that
embeds one short fixed probe (`"lambo keep-warm probe"`) every interval and
discards the vector. Knob: `[embedder] keep_warm_secs` /
`LAMBO_EMBED_KEEP_WARM_SECS`. Omitted = auto (30 s for candle on Metal, off
elsewhere), `0` = off, `N` = every N s for any kind. Code:
`src/embed/keep_warm.rs`, wired in `src/mcp/serve.rs` beside the ledger
heartbeat, resolved by `ResolvedBackends::keep_warm_interval()`.

**Acceptance status:** the issue's acceptance (p95 < 1 s on the live rig, p50
at > 30 min within 2x of the isolated probe) is **unconfirmed**. It has not
been measured on the re-pinned writer; the evidence below is two fresh probe
processes under heavy swap, n = 1 per row. Keep-warm keeps recently touched
weights resident and reduces how often a call pays the swap-in; it does not
guarantee residency under heavy memory pressure (see the worst-case table).

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
   operator change, not code: `ProcessType = Interactive` in
   `dev.lambo.dogfood.plist`. Not measured.
   * **Interactive, not Adaptive.** The man page defines Adaptive as moving
     between Background and Interactive "based on activity over XPC
     connections". `lambo serve` takes its calls over stdio, a unix socket or
     HTTP and opens no XPC transactions, so an Adaptive job would most likely
     sit in Background, which is a *stricter* class than the unspecified
     default. Interactive is the only value that removes the throttling.
   * **Timer coalescing.** launchd coalesces a job's timers by default, and
     `LegacyTimers = true` (precise timers) "may have no effect" unless the
     job is Interactive. So the 30 s keep-warm sleep may fire late by the
     coalescing leeway. Harmless: the period only has to sit well inside the
     < 2 min bucket, and a touch a few seconds late still does.

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

Reading: keep-warm removed the gap-growing component in these samples
(150-206 ms down to 54-59 ms at 60-420 s, which is about the 5 s-gap figure;
but see the worst-case table below: a call late in the period can still pay
it); what remains is the
~30 ms wake cost a 5 s gap already shows, which an interval cannot reach. The
off arm's 60 s figure (175 ms) is close to the live rig's < 2 min bucket
(186 ms p50).

Not measured: gaps beyond 7 minutes, the > 30 min bucket and its 9 s p90
(a fresh process does not age the way a 21 h writer under 8 GB of swap does),
and anything on the live writer itself. The store and graph side of a real
recall was not in this probe (embed only).

### Worst case and a better residency metric (review follow-up, 2026-10-07)

The A/B above never placed a call just before the next touch. Same method
(two concurrent fresh processes, candle on Metal, offline cached weights, the
same 14-word query, n = 1 per row), with the keep-warm arm wrapping the
embedder so the probe knows when each touch finished and times the call at
an exact age since the last touch. System swap was **8.9-9.0 of 10 GB**, more
pressure than the first run (8.2/9.2). Before each call the probe ran
`footprint --swapped` on itself and read the **`IOAccelerator (graphics)`**
row, which is the Metal weight buffers (~1.08 GB dirty).

| idle before the call | keep-warm off: first ms / gfx swapped | keep-warm 30 s: first ms / gfx swapped |
|---|---|---|
| 5 s | 43.6 / 0.3 MB | 72.5 / 0 B |
| 10 s | 179.6 / 917 MB | 45.7 / 0 B |
| 15 s | 58.2 / 64 MB | 59.0 / 0 B |
| 20 s | 181.8 / 1079 MB | 79.7 / 0.7 MB |
| 25 s | 113.0 / 6 MB (at 21 s) | **198.2 / 1082 MB** (at 21 s) |
| 28 s | 192.6 | **261.5** |
| 29 s | 172.3 | **190.8** |
| 29 s | 195.7 | 64.4 |

("Idle" is since the last query for the off arm and since the last touch for
the keep-warm arm. Second calls were 19-21 ms throughout. The 25-29 s rows
had a snapshot only before the first call.)

Readings:

1. **`footprint`'s `IOAccelerator (graphics)` Swapped column is the
   residency metric `top` CMPRS is not.** It tracks latency sample by sample:
   0 B swapped goes with 46-80 ms, ~1 GB swapped with 180-260 ms. Over the
   same samples `top` CMPRS sat at 1.27-1.44 GB in *both* arms, including
   the rows where the weight buffers had 0 B swapped. `vmmap --summary`
   shows the same row (SWAPPED 1.1 G on the 21 s keep-warm sample, 6 MB on
   the off arm's). Use `footprint --swapped <pid>` for the live-rig check
   and for the issue's acceptance item in place of CMPRS.
2. **Under this pressure the pager takes the whole weight set within 10-21 s
   of last use**, stochastically (the off arm's 15 s sample kept it, its 10 s
   one lost 917 MB). So a 30 s interval protects calls that land within
   ~20 s of a touch and **not** the last third of the period: calls at
   21-29 s paid 191-262 ms in 3 of 4 samples, no better than off. Averaged
   over a uniform arrival phase, 30 s still helps (5-20 s rows: 46-80 ms
   against 44-182 ms off), but the worst case is not bounded by it.
3. Implication for the default, **not acted on here** (it is a design change
   and n = 1): under heavy swap the interval would need to be ~10 s to keep
   the buffers resident through the whole period; at ~17 ms per probe that is
   ~0.2% GPU duty, still small. Pressure on the live rig varies, so the
   cheaper confirmation is `footprint --swapped` on the re-pinned writer
   before choosing a number.

## Design

* **What a touch does:** one minimal forward through the same embedder
  instance the session uses (`Memory::embedder()`). It writes nothing: no store
  I/O, no graph mutation, no ledger line, no recall-cache entry. It is not part
  of the embedding contract.
* **Where:** spawned on the holder path below the shutdown arming, beside the
  heartbeat, before the serve-level "session attached" line. Spawning awaits
  nothing, so the pre-handshake window is unchanged; the first touch is one
  full interval after arming, so startup gains no forward. Aborted as soon as
  the transport returns, before the close and its final drain (review nit;
  `run_and_close`'s `stop_before_close`), and again beside the heartbeat after
  it. Proxies and one-shot CLI commands never run it. A proxy also no longer
  *holds* a model: before the review fix, `serve` kept the resolved builder
  (embedder included, ~1.1 GB on Metal) alive through the whole proxy arm;
  `resolve_role` now takes it by value. Off for fixture builds unless
  `keep_warm_secs` is set, so `serve_pre_handshake_durability` runs the same
  timeline as before; `tests/serve_keep_warm_wiring.rs` sets it to 1 to pin
  holder-arms / proxy-does-not.
* **Why one tiny forward is enough:** on Metal, residency is per buffer per
  command buffer, so every weight buffer a kernel binds is made resident
  whole, the ~512 MB word-embedding table included, though the probe gathers
  only a few of its rows. On CPU candle or llama.cpp a touch reads only the
  pages it uses, so most of that table stays cold there.
* **Concurrency:** a touch that meets a real query in the candle coalescer
  joins its batch, padded to the longest member (`BatchLongest`): two rows at
  the query's length, and the query vector may differ by f16 batching noise,
  exactly as with any two concurrent real calls.
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
  **Qualified by the worst-case table:** under 9 GB of swap the buffers were
  lost within 10-21 s of last use, so 30 s does not cover the last third of
  each period. Kept for now; see follow-ups.
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
* The `ProcessType = Interactive` A/B on the launchd job (operator change;
  see Diagnosis item 4 for why not Adaptive).
* Replace the issue's `CMPRS` acceptance item with latency by gap plus the
  `footprint --swapped` `IOAccelerator (graphics)` row.
* Decide whether the 30 s auto interval should drop (~10 s) given the worst-
  case table above; measure on the live writer first.
