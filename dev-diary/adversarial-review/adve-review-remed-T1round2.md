# Adversarial Review: Remediation T1 — Round 2 (branch `remed/T1`)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 2 of the review/remediate loop                 ║
║  Scope:  Re-review the FULL remediated diff end to end.              ║
║          R1-1, R1-3, R1-4, R1-5 were remediated; R1-2 deferred to T2 ║
║          (documented disposition — NOT a defect).                    ║
║  Branch: remed/T1 (worktree /home/nryn/work/worktrees/remed-T1)      ║
║  Reviewer: T1ReviewR2 (read-only)                                    ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 3 P3 / 2 nits.                     ║
║          Core T1 guarantee holds; two P3s are remediation items      ║
║          CLAIMED fixed but not actually delivered (see P3-2/P3-3).   ║
╚══════════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only in the `remed/T1` worktree. Re-read the round-1 review
(`dev-diary/adversarial-review/adve-review-remed-T1round1.md`) and the full
`git diff` (`src/config.rs` +139, `src/memory.rs` +35, `src/resolve.rs` +3), then
re-read every changed region in context (`src/config.rs` validate + tests,
`src/memory.rs` build + new lease test, `src/resolve.rs` resolve_backends). Enumerated
all callers of `resolve_backends` / `resolve_from_config_path` / `resolve_store_only`
and traced the default `[daemon]` config values. No source was edited. Ran the three
new config tests and the new memory lease test — **all pass**. Main owns the full suite /
formatter / clippy.

## Round-1 finding disposition (verified)

| Finding | Verdict in round 1 | Round-2 disposition |
|---------|--------------------|---------------------|
| T1-R1-1 (P3) resolve boundary doesn't validate | P3 | **Partially fixed** — see P3-1 (validate added, but *after* store/embedder build, and the comment lies about it) |
| T1-R1-2 (P3) `.backends()` drops `backends.config` | P3 | Deferred to T2 — documented disposition, **expected/known, not a defect** |
| T1-R1-3 (P3) test coverage gaps (builder + TOML) | P3 | **Fixed** — new `build_rejects_zero_cadence_before_acquiring_the_lease` (memory) + new `lambo_file_zero_daemon_cadences_fail_validate` (config); see P3-2 for an overclaim in the lease test's doc comment |
| T1-R1-4 (nit) validate() comment over-generalizes gc_interval | nit | **Fixed** — `src/config.rs:193-195` now correctly splits the rationale (`gc_interval` mutation-counter vs the three `Duration` `tokio::interval` feeders) |
| T1-R1-5 (nit) `.as_secs_f64()` always renders 0 | nit | **NOT fixed** — see P3-3: the `as_secs_f64()` calls remain at `config.rs:205/211/217` unchanged |

## Findings

### P3

#### T1-R2-1 (P3) — `resolve_backends` comment falsely claims validate runs "before store/embedder are built"; R1-1's fail-fast-before-heavy-construction is only partially delivered
- **Where:** `src/resolve.rs:101-103` (comment), `src/resolve.rs:79-81` + `103` (order).
- **What:** The added comment reads "Fail closed at the file boundary … uniformly and
  **before store/embedder are built**." The code does the opposite: `build_store` (R-79)
  and `build_embedder` (R-80/81) run first, then `config.validate()?` (R-103) runs after
  the store/embedder dimension checks. Nothing about `validate()` depends on the
  constructed store/embedder, so it could trivially be moved above line 79.
- **Why it matters:** R1-1 explicitly asked for the rejection to happen before the
  (possibly heavy) embedder construction — an embedder build may load a model. The
  fail-closed-for-all-commands guarantee IS delivered (a bad file is now rejected for
  `derive`/`inspect`/`saints`/`serve`, not just `Memory`), so there is no correctness or
  panic regression — but the heavyweight work still happens before the rejection, and the
  comment materially misstates the actual order. This is a wrong comment + a missed
  optimization, not a runtime defect.
- **Concrete fix:** Move `config.validate()?` to immediately after `daemon_cfg.apply_to(&mut config)`
  (above `build_store`/`build_embedder`), or at minimum correct the comment to say
  "…uniformly, before `ResolvedBackends` is returned" and drop the "before store/embedder
  are built" clause.

#### T1-R2-2 (P3) — lease test's doc comment overclaims "never reached the lease acquire"; a leaked lease would not be detected
- **Where:** `src/memory.rs:3457-3461` (doc comment) and `src/memory.rs:3479-3481` (assert comment).
- **What:** The test asserts `build()` returns `LamboError::Config(_)` for `gc_interval: 0` on a
  fresh `MemoryStore`, and the comment claims this "proves we stopped at validate() and never
  reached the lease acquire." It does **not**. On a fresh store `acquire_lease` succeeds without
  error, so if `validate()` were moved to *after* the lease acquire, the build would still
  acquire the lease, then hit validate and return the same `Config` error — the test would pass
  either way. The test therefore pins "validate() is reachable in build()" (removing validate
  makes build succeed and the test fails — verified non-vacuous at the Config-error level) but
  does **not** pin the validate-before-lease ordering, and a held/leaked lease is **not**
  detectable by this test.
- **Why it matters:** The implementation is correct (validate genuinely precedes the lease at
  `memory.rs:565` vs `:589`), so this is a test/documentation overclaim, not a broken guarantee.
  But the round-2 brief explicitly asked to verify the test "genuinely proves the lease was never
  acquired (a leak would be detectable)" — it does not, and the comment asserts otherwise.
- **Concrete fix:** Tighten the claim ("a `Config` error from `build()` pins that `validate()`
  rejects before any spawn" — drop "never reached the lease acquire" / "proves … never reached
  the lease acquire"), or make the test actually detect a leak by following the failed build with a
  second `build()` on the same store/session/agent using a valid config and asserting it succeeds
  (a leaked held lease from a validate-after-lease regression would wedge that second build).

#### T1-R2-3 (P3) — R1-5 not actually fixed: `.as_secs_f64()` still present in all three Duration error messages
- **Where:** `src/config.rs:205`, `src/config.rs:211`, `src/config.rs:217`.
- **What:** The remediation brief lists "R1-5: three Duration error messages changed to drop the
  always-0 `as_secs_f64()` value." The messages are **unchanged** from round 1: all three still end
  in `got {}` with argument `….as_secs_f64()`, which is literally always `0` for `Duration::ZERO`
  (the only value that can reach here). Rendering "got 0" is not clean/consistent with the
  `gc_interval` message's plain `{}` (`config.rs:198`), which was exactly the round-1 nit.
- **Why it matters:** Cosmetic (a correct-but-ugly error string), but it is a remediation item that
  was explicitly claimed done and is demonstrably not. Flagging so it is not silently dropped.
- **Concrete fix:** Use `{:?}` (renders `0ns`) or drop the value, matching the `gc_interval` branch's
  plain style.

### Nits

#### T1-R2-4 (nit) — the "each override alone is also rejected" claim lives in the wrong test
- **Where:** `src/config.rs:465` (comment in `daemon_config_zero_cadence_override_rejected`). `src/config.rs:525-546` (the actual alone-cases).
- **What:** The brief says `daemon_config_zero_cadence_override_rejected` "was extended to include
  gc-only-alone." It was not — that test covers combined (gc=0, canon=0) and `only_canon` (canon=0,
  gc unset), plus the positive sanity case, but no gc-only case. The gc-only-alone case lives in the
  separate TOML test `lambo_file_zero_daemon_cadences_fail_validate` (`config.rs:525-534`), so the
  coverage gap R1-3 flagged is genuinely filled — just in a different test than the brief credits.
  The in-test comment "Each override alone is also rejected" is only true when read together with
  the TOML test.
- **Concrete fix:** Either move the gc-only-alone case into `daemon_config_zero_cadence_override_rejected`
  (matching the claim), or narrow/reposition the comment.

#### T1-R2-5 (nit) — `resolve_store_only` path still accepts a degenerate `[daemon]` file
- **Where:** `src/resolve.rs:124-129` (`resolve_store_only`).
- **What:** `provision` (and reader tools that don't embed) resolve via `resolve_store_only`, which
  does not `validate()`. A `[daemon] gc_interval = 0` file routed through a store-only command still
  resolves silently. This is consistent with R1-1's scoping (only `resolve_backends` was asked to
  fail closed) and store-only commands never run a daemon interval, so it is not a panic path — but
  it is an asymmetry worth a one-line note so a later reader does not assume every file-driven
  command rejects a bad `[daemon]`.
- **Concrete fix:** Document the asymmetry (one sentence) or, if uniformity is desired, validate in
  `resolve_store_only` too.

## Regression scan (checked, no defect found)

- **Fail-closed for all full-resolve commands:** `resolve_from_config_path` propagates the new
  `resolve_backends` error; `command_needs_resolve` → `resolve_from_config_path` (`src/main.rs:319`)
  and `resolve_serve_backends` (`src/mcp/serve.rs:656-658`) now reject a bad `[daemon]` at resolve
  instead of only at `Memory::build()`. No existing caller builds a full resolve with a zero cadence
  expecting success: the resolve tests (`resolve_memory_plus_fixture_any_configured_dim`,
  `resolve_backends_default_memory_and_bge`, `resolve` in `cli/mod.rs`/`saints.rs`) all use
  store/embedder-only files or `daemon: Default::default()` → defaults, which validate. The lib
  doc-test `resolve_backends(LamboFile::default())` also defaults → valid.
- **Shipped example not regressed:** `lambo.example.toml` keeps the entire `[daemon]` section
  commented out, so the example validates under the new check.
- **Defaults still valid** (`src/config.rs:137-158`): `backend_flush_interval=1s`,
  `daemon_tick_interval=DAEMON_TICK_INTERVAL`, `gc_interval=10_000`,
  `canonization_eval_interval=60s` — all pass `validate()`, so `lambo demo` and every default-path
  command are unaffected.
- **Core T1 guarantee intact:** `MemoryBuilder::build()` still runs `config.validate()?` on the
  merged config (named setters applied at `memory.rs:552-560`) at `memory.rs:565`, before the lease
  (`:589`), the startup load, and every daemon/flush/canon spawn. Merged-config rejection
  (`.flush_interval(Duration::ZERO)`) is preserved. The three `Duration` interval feeders and the
  `gc_interval` mutation counter are all covered; no config-fed `tokio::interval` escapes.

## Summary

The remediation delivers most of what round 1 asked: R1-3 is genuinely fixed (new builder-level
lease test + new TOML-file-driven test, both passing and non-vacuous), R1-4 is cleanly reworded,
and R1-1's fail-closed-for-all-commands behavior is now in `resolve_backends`. Two items are
claimed-but-not-actually-delivered and must not be lost: **R1-5 is not fixed** (`as_secs_f64()` is
still in the three error messages) and **R1-1's fail-before-heavy-construction is not delivered**
— validate runs after the store/embedder are built, and the new comment falsely states the opposite
(P3-1/P3-3). The lease test's doc comment overclaims its proof strength (P3-2). None of these
defeat the task's goal: the daemon `tokio::interval` zero-period panic stays closed at runtime, the
merged-config and demo paths are unaffected, and there is no P1/P2. R1-2's deferral to T2 is the
single known, documented (non-defect) item. Verdict: **APPROVE**, with the P3s recommended for
cleanup.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T1-R2-1", "T1-R2-2", "T1-R2-3"], "nits": ["T1-R2-4", "T1-R2-5"] }, "summary": "Round-2 re-review of the remediated T1 worktree. Core T1 guarantee holds: validate() runs on the merged config in MemoryBuilder::build() before the lease/load/spawn, all three tokio::interval cadences plus gc_interval are covered, and resolve_backends now fails closed for every full-resolve command (derive/inspect/saints/serve) with no regression to existing tests, the example config, or the default/demo path. R1-3 fixed (new builder lease test + TOML zero-cadence test, all passing), R1-4 reworded correctly, R1-2 deferred to T2 (documented, not a defect). Three P3s: (T1-R2-1) resolve.rs comment falsely claims validation happens 'before store/embedder are built' while build_store/build_embedder actually run first — validate could (and per R1-1 should) be moved above them, defeating part of the fail-fast benefit; (T1-R2-2) the lease test's comment overclaims it proves 'never reached the lease acquire' — on a fresh store a leaked lease is undetectable and validate-after-lease would still pass the test; (T1-R2-3) R1-5 is NOT fixed — the always-0 as_secs_f64() remains in all three Duration error messages. Two nits (alone-case coverage landed in the TOML test rather than the claimed test; resolve_store_only asymmetry undocumented). No P1/P2, no regression found: APPROVE." }
```
