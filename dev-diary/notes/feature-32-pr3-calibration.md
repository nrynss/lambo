# #32 PR 3: process-wide `EmbedderCalibration` (decisions)

Base: main `163d54e` (PR 2 merged, #11 merged in PR #61). Design of record:
the approved #32 design, §3.3's table row ("write-queue calibration probe |
process-wide | `EmbedderCalibration` passed through `MemoryBuilder`"), §5,
decision 14 and the PR 3 row of §8; #11's note
(`fix-11-writeq-drain.md`, "For #32 PR 3") for the shape of the probe it
left behind.

## What changed

The write queue's startup probe measures the **embedder**, but every
`WritePipeline` spawned its own `EmbedderProbe`. With many sessions per
process over one shared embedder (PR 4), every attach would fire
`PROBE_EMBEDS` (12) forwards at the same model to learn the same number.

- `writeq::EmbedderCalibration` (public, `Clone`, `Default`) holds one
  `EmbedderProbe` per embedder. `MemoryBuilder::calibration(c)` opts a
  build in. The first pipeline built over an embedder spawns that
  embedder's probe; every later one gets an `Arc` to it and fires no probe
  embed.
- A pipeline holds a `PipelineProbe`: `Owned(EmbedderProbe)` when built
  without a calibration (today's behaviour, unchanged), or
  `Shared { calibration, probe }`.
- `serve()` creates one calibration in its pre-lease group and passes it
  through `serve_builder` and to `ProcessTasks::spawn`; stage 2
  (`ProcessTasks::stop_before_close`) aborts its probe beside the
  keep-warm, stage 5 (`ProcessTasks::stop`) again.

`lambo_stats` reads `WritePipeline::calibration()`, which still combines the
probe's figure with this pipeline's own `ObservedRate`, so the probe fields
are the shared probe's and the observed fields and the apply-latency window
stay per session. The probe still times a representative two-concept derive
through `hybrid::context_text` over `hybrid::derive_prompt` (#11's framing),
untouched.

## Decisions

**Keyed by embedder identity, held weakly.** One calibration can be handed
builders over different embedders (a library caller, or a future serve with
more than one); it keeps a probe per embedder rather than reporting one
embedder's figure for another, or refusing. The key is the embedder `Arc`'s
allocation, compared with `std::ptr::addr_eq` (data pointer only, so two
vtables for one type cannot split it), and stored as a `Weak<dyn Embedder>`.
Weak because `proxy_releases_the_model` pins that a serve which loses the
election and proxies drops its model: a calibration that held the embedder
strongly would keep ~1.1 GB of candle Metal weights alive in every proxy. A
`Weak` keeps the allocation, so the address cannot be reused while the
entry exists; an entry whose embedder is gone is pruned at the next lookup
(its probe task, which held the embedder, has ended).

**Lazy, not eager.** The calibration spawns nothing when created. The first
pipeline build spawns the probe, which is when the probe always started
(inside `build_attach`, after the lease is taken). Creating the calibration
in `serve()`'s pre-lease group therefore costs nothing, and a proxying serve,
which builds no pipeline, never probes. Pinned:
`proxy_releases_the_model` now hands the proxy builder a calibration and
asserts it spawned no probe.

**The owner aborts, and so does the last drop.** #11 asked to move the
abort from the pipeline's close and `Drop` (`abort_probe`) to the owner, so
one session closing cannot abort a probe others read. `abort_probe` now
aborts only an `Owned` probe. A shared probe is aborted by
`EmbedderCalibration::abort` (serve: stages 2 and 5) and by the drop of the
calibration's last clone. Each `Shared` pipeline holds a clone, so a library
caller that passes `EmbedderCalibration::new()` inline (keeping no handle)
does not lose the probe when `build` returns; once every session holding it
is gone the probe stops. A shared probe that is aborted before it publishes
leaves every reader with `None`, exactly as an owned probe aborted at close
did.

**Stage 2, not a new stage.** The shared probe is aborted at stage 2
beside the keep-warm. Abort is instant, so stage 2 stays instant and the
#40 stage lines and watchdog bounds are unchanged (`shutdown stage 2/7
keep_warm_abort` keeps its name; renaming it would change a logged line
for no reader). Before this PR the probe was aborted one step later, at
the close's `replay_stop` step inside stage 3; both points are after the
transport has stopped, so no client can observe the difference.

**`ProcessTasks` holds the calibration; stage 2 is a hook (review P3-1,
P3-2).** The first cut extended `ProcessTasks::stop_before_close()`'s
handle list with `EmbedderCalibration::abort_handles()` inline in
`serve()` and aborted again inline at stage 5, leaving `ProcessTasks`'s
signatures alone for PR 4. The review found two problems. Nothing tested
serve's wiring (deleting either line kept every test green). And the
handle list was taken before the transport started, so a probe spawned
by an attach during the transport (PR 4's lazy attach) would escape
stage 2 and run across the closes. So:
- `ProcessTasks` holds a clone of the calibration (`spawn` takes it).
  `stop_before_close()` now aborts rather than returning handles: the
  keep-warm, then `EmbedderCalibration::shutdown()` (final; see the
  Sonnet review's L1 below). `stop()` shuts it down again. The serve tests drive exactly these methods.
- `run_and_close_sessions` takes stage 2 as an `impl FnOnce()` hook it
  runs when the transport returns, so the calibration is asked for its
  probes at stage 2. The `run_and_close` test seam keeps its handle
  slice. `abort_handles` is gone.

**Default unchanged.** `MemoryBuilder` without `calibration` spawns a probe
owned by the pipeline, aborted at its close and on `Drop`, as before. That
covers every CLI writer, every library caller, every existing test and
`build_memory` (the library entry point, which passes `None`).

**Log lines name their scope (review P3-4).** The probe's three log lines
carried `session = <id>`. They keep it and gain `scope`, a bare word:
`scope=session` for an owned probe, `scope=process` for a shared one,
where `session` names the session whose attach started the probe (in a
one-session serve, its session, so the dogfood writer's probe line still
names it). The first cut put both in one value (`scope=session <id>`),
which a `key=value` parser splits, and dropped the id for a shared probe.
The failure line says who goes without probe telemetry. CHANGELOG
(Changed) records the added field.

**A failed or aborted shared probe is retried (review P3-3).** Decision
14 says "once per process", but a probe that failed (a remote embedder
timing out during the first attach) would otherwise be terminal for
every later attach, for the life of the process. Each embedder's entry is
a `ProbeSlot` holding its current probe. When that probe has ended
without a measurement (it failed, or was aborted before publishing), the
next build over the embedder spawns a new one, provided
`PROBE_RETRY_BACKOFF` (60 s, public beside the other `PROBE_*`
constants) has passed since the last one ended (published or was
aborted; it counted from the start until the Sonnet review's L2). Bounded both ways:
only an ended probe is replaced, so at most one runs per embedder, and
an embedder that stays down is probed at most once a minute rather than
at every attach. A measured probe is never repeated. Pipelines hold the
slot, so earlier sessions see the re-probe's figure too, and the last
published (unmeasured) figure until it lands. The calibration's owner
shutting it down is final: `ProcessTasks` calls
`EmbedderCalibration::shutdown` at stages 2 and 5, and a build after it
gets a slot that never probes (Sonnet review L1), while the public
`abort()` stays re-probeable.

**A kept calibration keeps a running probe (review P3-6).** Closing the
sessions does not stop a shared probe, and the probe task holds the
embedder until it ends (at most `PROBE_WARMUP_BUDGET` plus
`PROBE_BUDGET`). Documented on `EmbedderCalibration` and
`MemoryBuilder::calibration`, with "call `abort()` when the last
session over an embedder goes".

**Builders with different configs share by embedder only.** The probe's
inputs are the embedder and constants (`PROBE_*`); nothing in `Config` or
the session reaches it, so sharing across sessions with different configs
is sound.

## Tests

- `memory::tests::calibration`: a second build with a shared calibration
  fires zero probe embeds (counting embedder), with a no-calibration control
  that does; a build during a running shared probe joins it (exactly
  `PROBE_EMBEDS` embeds); one session's close leaves the shared probe
  running for the other; an owned probe is still aborted by its close; the
  owner's `abort` stops a shared probe (idempotent); the probe lives while
  any holder does and stops at the last drop; one calibration keeps a probe
  per embedder; the probe line logs `scope` and `session` as plain fields;
  a failed shared probe is re-probed by the next attach (and a measured
  one is not); no re-probe inside the backoff; an aborted probe is
  re-probed once while two attaches race it.
- `mcp::server::tests::stats::a_shared_calibration_keeps_the_probe_fields_and_per_session_observations`:
  the single-session `lambo_stats` probe assertions hold for a shared probe;
  two sessions report the same probe figures and their own applied counts
  and apply-latency samples; the one past `OBSERVED_MIN_SAMPLES` reports
  `observed` and a non-null `probe_optimism`, the other `probe` and null.
- `mcp::serve::tests::calibration`: two sessions from one `serve_builder`
  (PR 4's template-builder shape) fire one probe; stage 2 through
  `ProcessTasks::stop_before_close` stops a running probe, including one
  spawned by an attach while the transport runs; stage 5 through
  `ProcessTasks::stop` stops one too.
- The "still running" preconditions hold by construction: the counting
  embedders let the first embed through and park the rest on a semaphore
  until the test releases it (review P3-5). No test depends on an embed
  delay.
- Mutation checks: with `PipelineProbe::new` ignoring the calibration, five
  of the first-cut tests fail; with `abort_if_owned` aborting a shared
  probe, the close test fails. Remediation: dropping the calibration from
  `ProcessTasks::stop_before_close` fails both stage-2 tests, and from
  `stop` the stage-5 test; removing the re-probe backoff check fails the
  backoff test; re-probing a measured probe fails the re-probe test.

Not covered by a test: that `serve()` itself passes its calibration to
`ProcessTasks::spawn` and calls the two methods (an in-process `serve()`
needs real signals and a transport). That is now one argument and the
two calls that also stop the keep-warm; the drop at `serve`'s return backs
it up.

## For PR 4

- Build every session's builder from the one `serve_builder` template (the
  calibration rides the clone), or pass the same calibration to each.
- Do not abort the calibration at a detach: other sessions read it. It is
  process-wide state, aborted at the process's stage 2 and 5.
- Stage 2 already reaches probes spawned during the transport (the
  calibration is asked at stage 2), so lazy attaches need no extra wiring.
- A failed probe is retried at the next attach after `PROBE_RETRY_BACKOFF`;
  the trigger session named on the retry's lines is that attach's.
