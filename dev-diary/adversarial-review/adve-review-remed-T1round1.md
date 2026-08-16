# Adversarial Review: Remediation T1 — `Config::validate()` cadence enforcement (branch `remed/T1`, round 1)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop           ║
║  Scope:  T1 — call Config::validate() in MemoryBuilder::build, ║
║          enforce gc_interval >= 1 and the three tokio::interval ║
║          cadences > 0, cover both new DaemonConfig fields      ║
║  Branch: remed/T1 (worktree /home/nryn/work/worktrees/remed-T1) ║
║  Date:   2026-08-16                                            ║
║  Reviewer: T1ReviewR1 (read-only)                              ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 3 P3 / 2 nits.               ║
║          The daemon panic is genuinely closed at runtime; the  ║
║          P3s are fail-fast/consistency and test-coverage        ║
║          hardening, none of them defeats the task's goal.      ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only in the `remed/T1` worktree. Full diff (`git diff`: `src/config.rs` +99,
`src/memory.rs` +4/0) read in context. Enumerated **every** `tokio::time::interval` /
`::interval(` call site in `src` and traced each interval source to its config field. Traced
the runtime config flow end to end (`LamboFile` → `resolve_backends` → `ResolvedBackends.config`
→ `Memory::builder().config(..)` → `build()` → `validate()`). No source was edited; no tests /
formatter / clippy run (Main owns final verification).

### Interval call-site census (all four sites in the crate)

| Site | Interval source | Config-backed? | Validated? |
|------|-----------------|----------------|------------|
| `src/daemon/mod.rs:671` `run_loop` | `tick` | `config.daemon_tick_interval` | ✅ (validated) |
| `src/canon/task.rs:232` `run` | `self.interval` | `config.canonization_eval_interval` | ✅ (validated) |
| `src/store/flush.rs:380` `run` | `self.params.interval` | `config.backend_flush_interval` | ✅ (validated) |
| `src/memory.rs:346` heartbeat | `LEASE_HEARTBEAT_INTERVAL` | const, not config | n/a (constant > 0) |

All three config-fed `tokio::interval` consumers are covered by the new checks; the fourth
feeds a compile-time constant. No config-fed interval is missed. tokio's `interval` panics iff
`period == Duration::ZERO` (its internal `assert!(period > Duration::ZERO)`), so the `==
Duration::ZERO` guards in `validate()` are exactly the panic condition — no over/under-guard.

### Runtime panic closure (traced, NOT just builder-local)

- `resolve_backends` (`src/resolve.rs:75`) applies `daemon_cfg.apply_to(&mut config)` into
  `ResolvedBackends.config` (resolved path: `resolve_from_config_path` → `resolve_backends`).
- The MCP serve path (`src/mcp/serve.rs:611-618`) forwards it: `.config(backends.config)
  .backends(backends).build().await` → `build()` runs `config.validate()?` at `src/memory.rs:565`,
  *before* the lease acquire (line 589), before the startup load, and before any
  `Daemon::from_config` / `flush.spawn` / `canon.spawn` / daemon `spawn` (lines 636-665). No side
  effect precedes it: the earlier lines (524-560) are pure required-field/`Option` unwraps and
  in-memory named-setter merges.
- The only non-`Memory` daemon construction is `src/cli/recall.rs:73`
  `Daemon::from_config(loaded.graph, &cfg)` — and it never `spawn()`s (recall drives
  `daemon.recall` directly; `reader_recall_does_not_spawn_gc_or_mutate_epoch` structurally
  forbids adding `.spawn()` there). `Daemon::from_config` itself creates no interval.
- Therefore **no daemon/flush/canon spawn that could hit `tokio::interval(ZERO)` is reachable
  without first passing `validate()`.** The `tokio::interval` panic is closed at runtime, not
  merely in the builder.

Placement is correct: `validate()` runs on the **merged** config (named-setter override at
`memory.rs:555-557` mutates `config.backend_flush_interval` *before* line 565), so a
`.flush_interval(Duration::ZERO)` is now rejected — as required. The merge happens after the
required-field unwraps but those produce no side effects, so "fail closed before any side
effect" holds.

### Branch semantics

- `gc_interval` is a **mutation counter** (`u64`), consumed in `daemon/mod.rs:865`
  (`epoch - last_gc_epoch >= params.gc_interval`); it does **not** feed a `tokio::interval`.
  `gc_interval == 0` would not panic but would run GC every cycle (degenerate runaway sweep).
  The `== 0` guard (≡ `>= 1` for `u64`) is therefore the correct, task-required check — the
  rationale is right even though the check is not itself a panic-preventer (see nit N1).
- `daemon_tick_interval` / `backend_flush_interval` / `canonization_eval_interval` are
  `Duration`s feeding `tokio::interval`; `== Duration::ZERO` is exactly the panic guard. Demo
  configs (`DEMO_TICK_INTERVAL=5ms`, `DEMO_FLUSH_INTERVAL=5ms`, `DEMO_GC_INTERVAL=1`,
  `BUILD/DEMO_EVAL_INTERVAL` non-zero) all pass, so `lambo demo` is not regressed.

### Other durations flagged by the brief

`conflict_recency_window`, `canonization_edge_min_age`, `canonization_repromotion_cooldown`
are consumed as *windows/comparisons* (daemon detection, `canon/stage2.rs`,
`canon/stage3.rs:76` cooldown), **not** as `tokio::interval` periods; zero would not panic.
`demo.rs` deliberately compresses `edge_min_age`/`repromotion_cooldown` to small non-zero
values, confirming they are meant to be settable low. Judged **not zero-hostile**; correctly out
of T1 scope. No finding required — recorded here so the remediator does not re-litigate.

## Findings

### P3

#### T1-R1-1 (P3) — `resolve_backends` / `resolve_from_config_path` do not validate; the file boundary admits a bad cadence silently on non-Memory commands
- **Where:** `src/resolve.rs:99-110` (`resolve_backends`), `src/resolve.rs:113-118` (`resolve_from_config_path`).
- **What:** Only `MemoryBuilder::build()` calls `validate()`. `resolve_backends` applies the
  `[daemon]` overrides and returns a `ResolvedBackends` without validating. Consequences:
  (a) commands that resolve once but never build a `Memory` (e.g. `derive`, `inspect`, `saints`
  via `resolve_from_config_path`) silently accept a `[daemon] gc_interval = 0` /
  `canonization_eval_interval_secs = 0` file; (b) on the serve path the (potentially heavy —
  embedder construction may load a model) store/embedder are fully built before `validate()`
  rejects, wasting work and delaying the failure.
- **Why it matters:** Not a panic (no daemon spawns without `build()`), and not a strict
  requirement of T1 (a), but it undercuts the "fail closed" story at the exact boundary the task
  names (the file that sets the cadence), and it makes the error surface inconsistently
  (later / command-dependent) instead of at config load.
- **Fix:** Call `config.validate()?` in `resolve_backends` immediately after `apply_to`
  (line 100), before building store/embedder (or at least before returning). This fails fast and
  uniformly for every file-driven command. Caveat: it changes behavior for non-daemon commands
  that currently tolerate a degenerate file — that is arguably the *desired* fail-closed
  direction, but note it explicitly if adopted.

#### T1-R1-2 (P3) — `.backends(ResolvedBackends)` drops `backends.config`; CLI writer commands never apply (or reject) `[daemon]` overrides
- **Where:** `src/memory.rs:452-458` (`backends`), `src/cli/mod.rs:67-70` (`open_writer`).
- **What:** `MemoryBuilder::backends()` copies store/embedder/embedding but **not** the
  `ResolvedBackends.config`. `open_writer` (used by CLI writer verbs) builds via
  `.backends(backends)` with no `.config(backends.config)`, so `build()` runs `validate()` on a
  *default* config. The MCP path explicitly passes `.config(backends.config)` (and a comment at
  `src/mcp/serve.rs:608-610` already warns "Without this the daemon always runs at
  `Config::default()`..."), but `open_writer` is the second, inconsistent site.
- **Why it matters:** This is **pre-existing and out of T1 scope** — it does not reintroduce a
  panic (defaults are valid), so the daemon panic stays closed. But it means a
  `[daemon] gc_interval = 0` file routed through a CLI writer would escape rejection entirely
  because the override is never applied, and the daemon cadence feature silently no-ops on that
  path. Worth a ticket / one-line `.config(backends.config)` so the fix's guarantee is uniform.
- **Fix:** In `open_writer` (or in `backends()`), propagate `backends.config` onto the builder,
  or explicitly document that writer commands ignore `[daemon]`.

#### T1-R1-3 (P3) — test coverage stops at `validate()`/`apply_to`; the two integration seams (builder wiring, TOML file) are unpinned, and the "each override alone" claim is partially untested
- **Where:** `src/config.rs:413-484`; `src/memory.rs` (builder tests).
- **What:** The two new tests are sound and cover every branch at the `Config`/
  `DaemonConfig.apply_to` level, but nothing exercises (a) the actual runtime entry —
  `Memory::builder().config(bad).build().await` returning a `Config` error *without* acquiring
  the lease — nor (b) the true file-driven flow: `LamboFile::from_toml_str("[daemon]
  gc_interval = 0 ...\n")` → `resolve_backends` → build. The wire-visible field name
  (`canonization_eval_interval_secs`, `u64`, `deny_unknown_fields`) is only parsed in
  `lambo_file_example_parses` with defaults, never with a zero override. Additionally the
  comment at `config.rs:464` ("Each override alone is also rejected") is only half-true: the
  canon-alone case is tested, but `gc_interval = Some(0)` **alone** (canon unset) is not — the
  gc branch is covered combined and at the `Config` level, so no branch is actually un-covered,
  just the "alone" claim over-states.
- **Why it matters:** The logic is correct and each branch is hit, so this is not a vacuous-test
  defect; it is robustness — the seams most likely to break the runtime guarantee (the builder
  calling `validate()` before lease, and the TOML field wiring) are not pinned, and a silent
  disconnect there would pass CI.
- **Fix:** Add one builder-level test (`Memory::builder().config(config_with_zero)
  .backends(..).build()` → assert `Config` error and that `LeaseHolder`/lease is never
  acquired), and one `LamboFile::from_toml_str("[daemon]\ngc_interval = 0\n...
  canonization_eval_interval_secs = 0\n")` → `apply_to` → `validate().is_err()` test. Fix the
  comment to say "the canon-only override is also rejected", or add the gc-only case.

### Nits

#### T1-R1-4 (nit) — `validate()` comment over-generalizes `gc_interval` as a `tokio::interval` feeder
- **Where:** `src/config.rs:193-194`.
- **What:** "Cadences feed tokio::interval, which panics on a zero period — fail closed here so
  a bad config cannot take down the daemon at startup." `gc_interval` is a mutation counter and
  feeds no `tokio::interval`; the panic rationale applies to only three of the four checks. The
  check itself is correct and task-required, but the comment should split the rationale
  (`gc_interval == 0` → runaway per-cycle GC, not a panic).
- **Fix:** Reword, e.g. "The three `Duration` cadences feed `tokio::interval`, which panics on a
  zero period; `gc_interval` is a mutation counter where 0 means GC every cycle — both must
  fail closed."

#### T1-R1-5 (nit) — `.as_secs_f64()` in the error messages always renders `0`
- **Where:** `src/config.rs:204, 210, 216`.
- **What:** Each branch only fires when the value `== Duration::ZERO`, so `as_secs_f64()`
  always formats to `0` ("got 0"). The f64 rendering implies a fractional non-zero is possible,
  which it can never be here, and is inconsistent with the `gc_interval` message (plain `{}`).
  No precision issue (the only value is exactly 0), purely clarity.
- **Fix:** Use `{:?}` (renders `0ns`) or drop the value from the message, to match the
  `gc_interval` branch's plain-`{}` style.

## Verified-OK (probed, not defects)

- Merged-config validation: `.flush_interval(Duration::ZERO)` sets
  `config.backend_flush_interval` before `validate()`, so it is rejected — confirmed by reading
  `memory.rs:555-557,565`.
- Demo not regressed: `build_config`/`canonization_config` keep all four cadences ≥ 1 / non-zero.
- No existing test or prod path constructs a `Config` with a zero validated cadence that would
  now spuriously panic/regress (the `Duration::ZERO` hits in `canon/{stage2,stage3,eval}.rs` are
  stage-function age arguments, unrelated to the four `Config` cadences).

## Summary

T1 is correctly and completely implemented: `Config::validate()` now rejects `gc_interval == 0`
and the three `tokio::interval`-feeding cadences at `Duration::ZERO`; every config-fed
`tokio::interval` consumer in the crate is covered and the only other interval is a
compile-time constant; and `config.validate()?` runs in `MemoryBuilder::build()` after the
named-setter merge and before the lease acquire / startup load / any spawn, closing the daemon
panic at runtime for every daemon-spawning path (including the file-driven MCP route). The
merged-config behavior and the demo path are unaffected. No P1/P2 defects. The three P3s are
fail-fast/consistency (validate at the `resolve_backends` boundary), a pre-existing
`backends()` config-drop on the CLI writer path, and test-coverage hardening (no builder-level
or TOML-file-driven rejection test, and a partially over-claimed comment); the two nits are a
comment and an error-formatting wording. None of these defeats the task's goal, so this round
is **APPROVE**; the P3/nits are recommended cleanups.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T1-R1-1", "T1-R1-2", "T1-R1-3"], "nits": ["T1-R1-4", "T1-R1-5"] }, "summary": "T1 soundly closes the tokio::interval zero-period daemon panic: all three config-fed interval consumers are validated (4th is a const), validate() runs in MemoryBuilder::build() on the merged config before any side effect/lease/spawn, and the file-driven MCP path reaches it at runtime. No P1/P2. P3s: (1) resolve_backends doesn't validate so non-Memory commands accept a degenerate file and store/embedder are built before rejection; (2) pre-existing .backends() drops backends.config so CLI writer commands never apply/reject [daemon] overrides; (3) test coverage lacks a builder-level fail-before-lease test and a TOML-file-driven zero-override test, plus a partially over-claimed 'each override alone' comment. Nits: comment over-generalizes gc_interval as a tokio::interval feeder; .as_secs_f64() always renders 0 in unreachable-in-practice messages." }
```
