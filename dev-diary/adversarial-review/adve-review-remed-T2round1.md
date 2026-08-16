# Adversarial Review: Remediation T2 — `[daemon]` means the same thing in every subcommand (worktree `remed-T2`, round 1)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop           ║
║  Scope:  T2 — make the `[daemon]` cadence overrides apply to   ║
║          every CLI writer subcommand, not just `serve`.        ║
║          Carries T1-R1-2 (P3): `.backends()` dropped           ║
║          `ResolvedBackends.config`; `open_writer()` never      ║
║          applied `[daemon]`.                                   ║
║  Branch: remed-T2 (worktree /home/nryn/work/worktrees/remed-T2)║
║  Date:   2026-08-16                                            ║
║  Reviewer: T2ReviewR1 (read-only)                              ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 2 P3 / 2 nits.              ║
║          The change is correct and complete; the P3s are       ║
║          test-coverage and root-cause-vs-call-site hardening.  ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only. Diff is a single 9-line hunk in `src/cli/mod.rs` (`git diff`:
`src/cli/mod.rs +9`). Read `open_writer` (`src/cli/mod.rs:63-84`), the
`MemoryBuilder` config/backends/build implementation (`src/memory.rs:391-620`),
`ResolvedBackends` + `resolve_backends` (`src/resolve.rs:12-113`), `Config` /
`DaemonConfig::apply_to` (`src/config.rs:91-132,242-261`), `build_memory`
(`src/mcp/serve.rs:595-620`), the demo Memory construction (`src/cli/demo.rs:955-975`),
the `Commands` enum + dispatch (`src/main.rs:23-277,314-329,387-622`), and the T2
section of `dev-diary/notes/remediation-tasks.md`. Targeted `cargo check --bin lambo`
passes. No source edited; no full suite / formatter / clippy (Main owns final
verification).

### Every Memory-construction path in the crate (write verbs in bold)

| Caller | Site | Path to Memory | Honours `[daemon]`? |
|--------|------|----------------|----------------------|
| **`serve`** | `src/mcp/serve.rs:612` (`build_memory`) | `.config(backends.config.clone()).backends(backends)` | ✅ (pre-existing, unchanged) |
| **`derive`** | `src/cli/derive.rs:87` → `open_writer` | `src/cli/mod.rs:75-80` | ✅ (this change) |
| **`record-action`** | `src/cli/record_action.rs:44` → `open_writer` | same | ✅ (this change) |
| **`reserve`** | `src/cli/reserve.rs:55` → `open_writer` | same | ✅ (this change) |
| **`release`** | `src/cli/reserve.rs:83` → `open_writer` | same | ✅ (this change) |
| **`demo`** | `src/cli/demo.rs:964` (own `open()` helper) | `.config(build_config()/canonization_config()).clock(script_clock())` — **not** `open_writer` | excluded by design (see below) |
| `recall` | no Memory writer | reader (drives `Daemon::from_config`, no spawn) | N/A |
| `saints` / `inspect` / `stats` | StoreOnly resolve | reader | N/A |
| `provision` | StoreOnly resolve | ops only, no Memory | N/A |
| `serve-web` | Full resolve but reader | `serve_web::run`, no Memory writer | N/A |

Production-only builder sites (excluding `#[cfg(test)]` modules): `open_writer`,
`build_memory`, and demo's `open()`. The `Memory::builder()` calls at
`src/cli/mod.rs:269`, `src/cli/saints.rs:166`, `src/mcp/server.rs:1191`,
`src/mcp/serve.rs:1930/1998/2017`, `src/memory.rs:2372` and friends are all inside
`#[cfg(test)]` modules — verified by reading their surrounding contexts. **No
writer path to a daemon session bypasses `open_writer` or `build_memory`**, so
every production writer that resolves a file now forwards `backends.config`.

### The crux — does `.config(..)` before `.backends(..)` actually take effect? (Verified, not assumed)

Reading `src/memory.rs`:

- `.config(config)` (line 505-508) sets `self.config = config`.
- `.backends(backends)` (line 452-457) assigns **only** `self.store`, `self.embedder`,
  `self.embedding`. **It does not touch `self.config`** — confirmed line by line.
  It consumes `backends` by value (moving those three fields out), which is why
  `backends.config` must be cloned *before* the call.
- `build()` (line 551) starts from `let mut config = self.config;` — that **is** the
  passed clone — then applies only the named setters (`match_strategy`,
  `flush_interval`, `scoring_weights`), all of which are `None` on the
  `open_writer` path. No `Config::default()` is re-applied anywhere in `build()`.

So the effective daemon config is exactly `backends.config`. The `.config()//.backends()`
ordering is irrelevant here precisely because the builder keeps config and the
store/embedder/embedding in separate fields (the doc comment at 403-405 and
500-504 spells this out: order-independent). **The change works.**

### Does `backends.config` actually carry the applied+validated `[daemon]` overrides?

`resolve_backends` (`src/resolve.rs:75-113`) starts from `Config::default()`,
applies `daemon_cfg.apply_to(&mut config)` (only `gc_interval` and
`canonization_eval_interval_secs` exist as `[daemon]` knobs — `config.rs:242-261`,
i.e. `cfg.gc_interval` / `cfg.canonization_eval_interval`), runs `config.validate()?`
(line 84, T1), then stores `config` in `ResolvedBackends.config` (line 111) with a
doc comment "Product config with any `[daemon]` cadence overrides already applied."
Every `open_writer` caller (`derive`/`record-action`/`reserve`/`release`) receives
`*backends` from the `Resolved::Full` branch of `main::resolve_for_command`
(`main.rs:314-329`), which goes through `resolve_from_config_path` →
`resolve_backends`. So the config is applied **and** T1-validated before it ever
reaches `open_writer`. `.clone()` is correct and necessary (backends is moved into
`.backends()`; `Config: Clone` — `config.rs:91`), and matches `build_memory` exactly.

### Validation interaction (T1)

No bypass and no double-apply of overrides (the config is passed once). T1's
`resolve_backends` validation (line 84) fails the file closed before `open_writer`
is even reached (resolution happens first in `main`). `build()` runs
`config.validate()` again (defense in depth, unchanged). A degenerate file
(`gc_interval = 0` / `canonization_eval_interval_secs = 0`) is rejected at
resolution; it cannot slip through on any verb.

### `serve` unchanged / no double-application

`build_memory` (`serve.rs:604-620`) already forwarded `backends.config` before T2;
the hunk does not touch it. Nothing in serve applies the override a second time
(the daemon reads `config.daemon_tick_interval` / `gc_interval` /
`canonization_eval_interval` once in `build()`).

### `demo` exclusion holds

`demo` builds its own `Config` via `build_config()` / `canonization_config()`
(`demo.rs:879-895`) with compressed cadences (`DEMO_TICK_INTERVAL`,
`DEMO_GC_INTERVAL`, `DEMO_EVAL_INTERVAL`) **and** its own `script_clock()`
(`demo.rs:938-946`), and constructs via its own `open()` helper — not `open_writer`.
It deliberately does not honour a user `[daemon]`: the whole point of the demo is
to show canonization working on an ordinary (compressed) session, which requires
`gc_interval` small (the config.rs module doc, `src/config.rs:235-241`, documents
that the default 10 000 makes promotion unreachable and that `demo` sets it to 1
internally). Honouring a user `gc_interval = 10000` would silently defeat that.
Consistent with serve's design intent: demo is a fixed scripted scenario, not an
operator writer. Its own configs pass `build()`'s `validate()`.

## Findings

### P3

#### T2-R1-1 (P3) — no regression test pins that CLI writer verbs honour the `[daemon]` cadence
- **Where:** `src/cli/mod.rs` tests module (the `backends_on` helper at `:221-243` hard-codes
  `config: crate::Config::default()`).
- **What:** The fix is verified by reading, but nothing in the suite would catch a regression —
  e.g. a future removal of `.config(config)` in `open_writer` (back to `Config::default()`),
  which is exactly the T1-R1-2 divergence this task exists to close. The existing CLI tests
  all pass a default config through `backends_on`, so they cannot distinguish "writer uses
  resolved config" from "writer uses default".
- **Why it matters:** The whole task is that `[daemon]` must mean the same thing on every
  verb; the only thing guarding that invariant today is a comment. `Memory::config()`
  (`src/memory.rs:918`, `pub`) makes the effective config directly observable.
- **Fix:** Add one `#[tokio::test]` in `src/cli/mod.rs` tests: build a `ResolvedBackends` via
  `backends_on` but mutate `backends.config.gc_interval = 17` (a non-default), call
  `open_writer(backends, session, agent)`, then `assert_eq!(mem.config().gc_interval, 17)` and
  `mem.close().await`. This fails iff `open_writer` falls back to `Config::default()` — the
  precise regression. (A serve-side equivalent already exists implicitly via `build_memory`'s
  own path being pinned by the daemon tests, so one CLI-side test suffices.)

#### T2-R1-2 (P3) — the fix patches the two call sites rather than the root cause; the config-drop invariant in `MemoryBuilder::backends()` remains
- **Where:** `src/memory.rs:452-457` (`backends`), fix at `src/cli/mod.rs:75-80` and the
  pre-existing `src/mcp/serve.rs:611-616`.
- **What:** T1-R1-2 named the root cause as `.backends()` copying store/embedder/embedding but
  not `backends.config`. T2 addresses it at the two production callers (`open_writer` +
  `build_memory`), which is the doc's stated preference ("wire the config through `open_writer`
  ... mirroring `build_memory`") and is correct for today's callers. But the builder method
  itself still silently drops the config: `backends.config` is consumed-and-discarded, so any
  *future* writer path that calls `.backends(..)` without a compensating `.config(..)` re-creates
  the silent divergence (a file that validates but behaves differently per verb).
- **Why it matters:** Not a defect in this change — every current production writer is covered —
  but the invariants ("resolved backend carries its config; a writer built from a resolved
  backend uses it") would be more robust enforced by the builder than by caller discipline,
  and this is the second time the drop has produced real fallout.
- **Fix (optional, not required for this round):** in `MemoryBuilder::backends()`, also do
  `self.config = backends.config;` (config is a plain `Clone` field, populated by
  `resolve_backends`), which removes the need for the `.clone()` at both call sites and makes
  the drop impossible-by-construction. Alternatively accept the current caller-level wiring and
  record the invariant in the `backends()` doc comment so future callers see it. Either way;
  the current code is correct.

### Nits

#### T2-R1-3 (nit) — `open_writer` comment over-claims "Every CLI write verb opens its one Memory through this site"
- **Where:** `src/cli/mod.rs:70`.
- **What:** `demo` is a write verb and does **not** open through `open_writer` (it has its own
  `open()` helper with a scripted config/clock), and `serve` opens through `build_memory`.
  The immediately following enumerating sentence correctly scopes the claim to
  "derive / record-action / reserve / release … the same way it applies to serve", so the
  meaning is intact — but the bare opening clause is imprecise.
- **Fix:** Re-word to "Every full-resolve CLI writer verb (`derive` / `record-action` /
  `reserve` / `release`) opens its one Memory through this site; `serve` and `demo` use their
  own builders." (Optional.)

#### T2-R1-4 (nit) — demo's deliberate non-honouring of a user `[daemon]` is reasoned but not stated at the site
- **Where:** `src/cli/demo.rs:878-895` (config builders) and `:963-975` (`open()`).
- **What:** The exclusion is sound (scripted cadence + scripted clock are the demo's point;
  see Grounding), and `src/config.rs:235-241` documents why demo compresses cadence internally.
  But `demo.rs` itself never says that a user `[daemon]` in `lambo.toml` is intentionally
  ignored in favour of the scripted config — a reviewer/operator reading `demo.rs` alone could
  mistake it for a latent T2 gap.
- **Fix (optional):** a one-line note on `build_config()` / `open()`: "The demo deliberately does
  not honour a user `[daemon]` — its own compressed cadence and scripted clock are required for
  the state-machine demonstration (see `config.rs`)."

## Verified-OK (probed, not defects)

- Confirmed `.config(..)` survives `.backends(..)`: `backends()` touches only store/embedder/
  embedding (`memory.rs:452-457`); `build()` base for the merged config is `self.config`
  (`memory.rs:551`), never a fresh `Config::default()`.
- `.clone()` is correct/necessary — `backends` is moved into `.backends()`, `Config: Clone` —
  and mirrors `build_memory` (`serve.rs:611`).
- All five full-resolve writer verbs (serve, derive, record-action, reserve, release) resolve
  once and forward the validated `[daemon]` config; readers and `provision` correctly never
  build a daemon-writing `Memory`.
- Demo configs remain validated by `build()`; demo is not regressed.

## Summary

T2 is correctly and completely implemented. The single hunk makes `open_writer` forward
`ResolvedBackends.config` into `MemoryBuilder` via `.config(config)` before `.backends(backends)`;
I verified the builder keeps config and store/embedder/embedding in separate fields and that
`build()` uses the passed config as the merged base (named setters only add on top, all `None`
here), so the effective daemon cadence is exactly the resolved config — the highest-risk
ordering question resolves in the change's favour. Every production writer to a daemon session
(every CLI write verb except the self-contained `demo` and the already-correct `serve`) now
honours `[daemon]` identically; readers and `provision` remain correctly N/A; `demo`'s scripted
config/clock exclusion is sound and consistent with serve's design intent; and T1's fail-closed
validation is neither bypassed nor double-applied. No P1/P2. The two P3s are hardening — no
regression test pins the new behaviour (T2-R1-1), and the root-cause config-drop in
`MemoryBuilder::backends()` is still caller-guarded rather than enforced by construction
(T2-R1-2). Two nits (comment precision move; demo's exclusion note). None defeat the task's
goal, so this round is **APPROVE**.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T2-R1-1", "T2-R1-2"], "nits": ["T2-R1-3", "T2-R1-4"] }, "summary": "T2 correctly closes the [daemon] divergence: open_writer now forwards ResolvedBackends.config (validated daemon overrides from resolve_backends) into MemoryBuilder via .config() before .backends(), and the builder is verified to keep config separate from store/embedder/embedding so the passed config survives into build() as the effective daemon cadence (no Config::default() re-applied, named setters all None). Every production writer to a daemon session - derive, record-action, reserve, release via open_writer; serve via the unchanged build_memory - now honours [daemon] identically; demo is legitimately excluded (own scripted config/clock) and readers/provision are N/A. T1 validation is neither bypassed nor double-applied. No P1/P2. P3s: (1) no regression test pins that a CLI writer honours [daemon] (backends_on hard-codes Config::default; a mem.config() assertion would catch re-divergence); (2) the root-cause config-drop in MemoryBuilder::backends() is patched at the two call sites rather than by construction, leaving the invariant caller-guarded. Nits: open_writer comment over-claims 'every CLI write verb' (demo/serve excluded), and demo's deliberate non-honouring of user [daemon] is not stated at the site. All are cleanups; APPROVE." }
```
