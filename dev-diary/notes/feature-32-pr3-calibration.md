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
  through `serve_builder`; stage 2 aborts its probe beside the keep-warm,
  stage 5 again.

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

**Stage 2, not a new stage.** The shared probe is aborted through the same
`stop_before_close` list as the keep-warm: `serve()` extends
`ProcessTasks::stop_before_close()` with `EmbedderCalibration::abort_handles()`.
Abort is instant, so stage 2 stays instant and the #40 stage lines and
watchdog bounds are unchanged (`shutdown stage 2/7 keep_warm_abort` keeps
its name; renaming it would change a logged line for no reader). Before
this PR the probe was aborted one step later, at the close's `replay_stop`
step inside stage 3; both points are after the transport has stopped, so
no client can observe the difference. `ProcessTasks`'s signature is left
alone: PR 4 changes `spawn` once, and this PR does not touch it.

**Default unchanged.** `MemoryBuilder` without `calibration` spawns a probe
owned by the pipeline, aborted at its close and on `Drop`, as before. That
covers every CLI writer, every library caller, every existing test and
`build_memory` (the library entry point, which passes `None`).

**Log lines name their scope.** The probe's three log lines carried
`session = <id>`. A shared probe's spawning session is an accident of
attach order, so the field is now `scope`, displaying `session <id>` for
an owned probe and `process` for a shared one; the failure line says who
goes without probe telemetry. No test or doc matched the old field.
CHANGELOG records it under Changed.

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
  per embedder.
- `mcp::server::tests::stats::a_shared_calibration_keeps_the_probe_fields_and_per_session_observations`:
  the single-session `lambo_stats` probe assertions hold for a shared probe;
  two sessions report the same probe figures and their own applied counts
  and apply-latency samples.
- `mcp::serve::tests::calibration`: two sessions from one `serve_builder`
  (PR 4's template-builder shape) fire one probe; stage 2 (`run_and_close`
  handed the calibration's abort handles) stops a running probe.
- Mutation checks: with `PipelineProbe::new` ignoring the calibration, six
  of the new tests fail; with `abort_if_owned` aborting a shared probe, the
  close test fails.

Not covered by a test: `serve()` itself extending stage 2 with the abort
handles (an in-process `serve()` needs real signals and a transport; the
stage tests drive `run_and_close`). The stage-5 abort and the drop at
`serve`'s return back it up.

## For PR 4

- Build every session's builder from the one `serve_builder` template (the
  calibration rides the clone), or pass the same calibration to each.
- Do not abort the calibration at a detach: other sessions read it. It is
  process-wide state, aborted at the process's stage 2 and 5.
