# Adversarial Review: Remediation T1 — Round 3 (branch `remed/T1`)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — Round 3 of the review/remediate loop               ║
║  Scope:  Re-verify the five R2 remediation items AND the full        ║
║          current diff end to end.                                    ║
║  Branch: remed/T1 (worktree /home/nryn/work/worktrees/remed-T1)      ║
║  Reviewer: T1ReviewR3 (read-only)                                    ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 0 P3 / 1 nit.                      ║
║          All five R2 findings are genuinely delivered; no fix        ║
║          regresses the core T1 guarantee. The single known           ║
║          non-defect remains R1-2 .backends().config drop, deferred   ║
║          to T2 (documented disposition).                             ║
╚══════════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only in the `remed/T1` worktree. Re-read round-2 and round-1 reviews
(`adve-review-remed-T1round1.md`, `adve-review-remed-T1round2.md`) and the full
`git diff` (`src/config.rs` +150, `src/resolve.rs` +12/-3, `src/memory.rs` +53). Re-read every
changed region in context: `resolve_backends` + `resolve_store_only` (`src/resolve.rs`),
`Config::validate()` + all three Duration error messages + `daemon_config_zero_cadence_override_rejected`
(`src/config.rs`), `MemoryBuilder::build()` + the rewritten lease test + `register_session`/
`close`/`LEASE_TTL` (`src/memory.rs`). Enumerated every caller of `resolve_backends` /
`resolve_from_config_path` / `resolve_store_only`. No source was edited. Ran `cargo check --quiet`
(passes) and the four targeted tests — `cadence_validation_fails_closed`,
`daemon_config_zero_cadence_override_rejected`, `lambo_file_zero_daemon_cadences_fail_validate`,
`build_rejects_zero_cadence_before_acquiring_the_lease` — all pass (722 tests filtered out; no
failures). Main owns the full suite / formatter / clippy.

## R2 finding disposition (all five verified)

| Finding (round 2) | Claimed fix | Verified |
|-------------------|-------------|----------|
| T1-R2-1 (P3) validate() not before store/embedder build; comment lied | Move `validate()?` above `build_store`/`build_embedder`; truthfully rewrite comment | **Genuinely fixed** — see AD-1 |
| T1-R2-2 (P3) lease test's doc comment overclaimed; a leak was not detectable | Add a second `build()` on the same store/session/agent with a valid config; rewrite doc comment | **Genuinely fixed** — see AD-2 |
| T1-R2-3 (P3) `.as_secs_f64()` still in the three Duration messages | Drop it; use `{:?}` | **Genuinely fixed** — see AD-3 |
| T1-R2-4 (nit) "Each override alone" claim lived in the wrong test | Add a gc-only-alone case to `daemon_config_zero_cadence_override_rejected` | **Genuinely fixed** — see AD-4 |
| T1-R2-5 (nit) `resolve_store_only` asymmetry undocumented | Add a one-sentence doc note | **Genuinely fixed** — see AD-5 |

### AD-1 — R2-1: `validate()` now genuinely precedes heavy construction (fixed)
- **`src/resolve.rs:79-84`:** `Config::default()` + `daemon_cfg.apply_to(&mut config)` +
  `config.validate()?` all sit **immediately after** `let daemon_cfg = file.daemon;` (line 78) and
  **before** `build_store` (line 85) / `build_embedder` (line 86). The claim in the round-2
  finding ("the store/embedder are fully built before validate") is now wrong; the order is exactly
  what round 1 asked for.
- **Does moving it break anything?** No. The config construction depends only on `Config::default()`
  and `daemon_cfg` (a field of `file` read at line 78); it does **not** depend on any
  `FileConfig` store/embedder field that is populated later. The built `config` value is reused
  unchanged at `resolve.rs:111` (`config,`) as it was before — the value is identical, only the
  validate call moved earlier. No `Config` default is validated differently; defaults still pass.
- **Error type unchanged:** `config.validate()?` still yields `LamboError::Config` (the same
  type the surrounding `build_store`/`build_embedder` failures are mapped to). No caller pattern
  changes.
- **Behavior change (intended):** if *both* the `[daemon]` cadence is degenerate *and* the
  store/embedder build would fail, the config error now surfaces first instead of the store/embedder
  error. This is the desired fail-fast direction and no caller depends on the old precedence.
- **Comment now truthful** (`resolve.rs:79-81`): "Fail closed at the file boundary … uniformly and
  BEFORE any store/embedder build (an embedder build may load a model …)". Accurate.

### AD-2 — R2-2: the lease test now genuinely detects a leaked lease (fixed)
- **`src/memory.rs:3465-3504`.** The test fails the first `build()` on `gc_interval: 0` and asserts
  `LamboError::Config(_)`, then performs a **second `build()` on the same store/session/agent
  (`"bad-cadence"`/`"agent-a"`) with a valid `Config`** and asserts it succeeds, then `close()`.
- **Why the second build detects a leak:** in the current code `validate()` runs at
  `memory.rs:565`, *before* `acquire_lease` (`:590`). A scan of `MemoryBuilder::build()` confirms
  the lease is the first side effect (lines 524-565 are pure required-field unwraps, Option checks,
  and in-memory named-setter merges; `config.validate()?` at 565 precedes line 589's acquire). If a
  regression moved `validate()` to *after* the lease acquire, the first build would acquire the
  `"bad-cadence"` lease (fresh store → `Acquired`), then fail at validate and return the same
  `Config` error — but with the lease left **held and unrefreshed** (`LeaseHolder` never reaches the
  heartbeat at line 668). The second build on the same session would then hit `LeaseOutcome::Held`
  at `acquire_lease` and return `LamboError::Conflict`, so its `.expect("a valid second build must
  succeed …")` would fault. Thus the test genuinely pins that no lease was leaked — it would fail
  under a validate-after-lease regression.
- **Session reuse validity:** a successful `close()` (line 1566) releases both the store lease and
  the in-process `ACTIVE_SESSIONS` slot (`unregister_session`), so issuing a fresh build on the same
  session after a clean close is the crate's ordinary, supported shape (the MCP server's reload
  does exactly this). The second build acquires the lease cleanly because the first never held it.
- **Second-writer / `ACTIVE_SESSIONS` interference:** `register_session` (`:686`) runs only *after*
  a successful lease acquire — the first (failing) build never reaches it, so it leaves no
  `ACTIVE_SESSIONS` entry for `"bad-cadence"`. That session/agent pair (grep) is unique to this test;
  no other test uses it. The optional `SecondSessionWriter` path would only fire if the two builds
  were *concurrently* live, which they never are (sequential, and the first never registers). No
  cross-test interference.
- **Lease TTL / determinism:** `LEASE_TTL` ≈ 30s (reference at `memory.rs:1442/1454`); the test runs
  in milliseconds, so even a leaked lease would not lapse within the test window — the `Held`
  detection is deterministic, not TTL-timing-dependent. The store is a test-local
  `MemoryStore::new()` (not a shared static), so isolation holds.
- **Non-vacuous:** the bare `Config`-error assertion alone would *not* distinguish validate-before-
  lease from validate-after-lease (a fresh-session after-lease validate also returns `Config`) — the
  **second build is the discriminator**, and it is present and sound. The doc comment (3457-3464)
  correctly describes this mechanism ("a validate-after-lease regression would have leaked a held
  lease and wedged it … `acquire_lease` -> `Held`"). The R2 overclaim is gone. (One residual
  wording nit at 3482-3483 — see NINT-1.)

### AD-3 — R2-3: `.as_secs_f64()` is gone from all three Duration messages (fixed)
- **`src/config.rs:202-219`:** all three branches now render the value with `{:?}`:
  `"daemon_tick_interval must be > 0, got {:?}"` (204/205), `"backend_flush_interval … got {:?}"`
  (210/211), `"canonization_eval_interval … got {:?}"` (216/217). A `Duration::ZERO` renders
  `0ns` — clean and no longer the always-`0` f64. `grep -n "as_secs_f64"` across `src/config.rs`,
  `src/memory.rs`, `src/resolve.rs` returns **zero** matches. Consistent with the `gc_interval`
  branch's plain-`{}` message (`config.rs:198`).

### AD-4 — R2-4: gc-only-alone case added, "Each override alone" now true (fixed)
- **`src/config.rs:465-486`:** `daemon_config_zero_cadence_override_rejected` now tests canon-alone
  (`:466-475`) *and* **gc-alone** (`:476-486`, `gc_interval: Some(0)` with canon unset), so the
  in-test comment "Each override alone is also rejected" (`:465`) is now genuinely true without
  relying on the separate TOML test. This is corroborated (with actual TOML text) by
  `lambo_file_zero_daemon_cadences_fail_validate` (`config.rs:525-558`), which also covers both
  alone-cases from real file strings. The R1-3 coverage gap is fully closed and the R2 nit's
  comment/coverage mismatch is resolved.

### AD-5 — R2-5: `resolve_store_only` asymmetry documented (fixed)
- **`src/resolve.rs:123-126`:** the doc comment now reads "Deliberately does NOT call
  `config.validate()`: store-only commands never run a daemon `[daemon]` interval, so a degenerate
  cadence is rejected only at the full `resolve_backends` boundary, not here." Accurate, and it
  records the deliberate asymmetry so a later reader will not assume every file-driven command
  rejects a bad `[daemon]`. Consistent with R1-1's scoping and not a defect (store-only commands
  never spawn a daemon interval).

## Core T1 guarantee (re-verified, intact)

- **Build-time validation before any side effect:** `config.validate()?` on the **merged** config
  (named setters applied at `memory.rs:551-560`) runs at `memory.rs:565`, before the lease acquire
  (`:589`), the startup load (`:618`), and every daemon/flush/canon spawn (`:663-665`). The
  merged-config rejection (`.flush_interval(Duration::ZERO)`) is preserved, so the R2-1 reorder in
  `resolve.rs` does not touch the builder's own fail-closed ordering.
- **All intervals covered:** the three config-fed `tokio::interval` cadences
  (`daemon_tick_interval`, `backend_flush_interval`, `canonization_eval_interval`) are guard-checked
  at `config.rs:202-219`; `gc_interval >= 1` (a `u64` mutation counter) at `config.rs:196-201`. The
  only other interval is the compile-time `LEASE_HEARTBEAT_INTERVAL` constant. No config-fed
  interval escapes.
- **File boundary fails closed for every full-resolve command:** `resolve_from_config_path`
  (`resolve.rs:116-121`) → `resolve_backends` now validates first; callers `main.rs:319`, the
  `cli/mod.rs`/`saints.rs` test resolves, and `mcp/serve.rs:657` all route through it. `cargo check`
  passes and the resolve tests use valid/default configs, so no caller regressed.
- **Store-only path (unchanged, documented):** `resolve_store_only` (`main.rs:326`,
  `cli/mod.rs:678`) still does not validate — intentional (see AD-5); those commands never spawn a
  daemon interval.
- **Defaults / demo unaffected:** defaults (`gc_interval=10_000`, `daemon_tick_interval=...`,
  `backend_flush_interval=1s`, `canonization_eval_interval=60s`) and the example config (entire
  `[daemon]` commented out) all validate.

## Regression scan

- **Full diff read end to end:** the only non-test/non-comment/non-message change is the
  `resolve.rs` reorder (AD-1), which is behavior-preserving except for the intended fail-fast error
  precedence on a doubly-bad config. No test or prod path depends on the old order.
- **R2 remediation introduced no new code paths:** the config.rs edits are test additions + error
  message rewording (assertion facility unchanged); the memory.rs edit is a rewritten test; the
  resolve.rs edits are the reorder + two doc lines.
- **`cargo check --quiet` passes**; all four targeted new/changed tests pass. Main owns the full
  suite / clippy / formatter.

## Findings

### P1 / P2
None.

### P3
None. All five round-2 findings are genuinely delivered and none regresses the core T1 guarantee.

### Nits

#### T1-R3-N1 (nit) — the lease test's inline assert comment over-attributes the "before the lease" proof to the single Config-error assertion
- **Where:** `src/memory.rs:3482-3483`.
- **What:** `"A Config error — not Conflict/Store/Held — pins that validate() rejects before any
  spawn and before the lease is acquired."` Standing alone, a lone `Config` error from `build()`
  does **not** by itself distinguish validate-before-lease from validate-after-lease on a fresh
  session: an after-lease validate on a free session also returns a `Config` error. The *second
  build* (`:3493-3503`) is what actually pins the lease-not-leaked property (see AD-2). This is the
  residual tail of the very overclaim round 2 flagged; the top-of-test doc comment (3457-3464) is
  fully accurate, so the fix is substantially landed.
- **Why it matters:** None functionally — the test's proof is sound and non-vacuous because of the
  second build. Purely a comment-precision nicety of the class this review loop watches for.
- **Fix:** Rephrase to attribute the proof to the second build, e.g. "A `Config` error (not
  `Conflict`/`Store`/`Held`) is consistent with validate rejecting before the lease; the second
  build below is what proves no lease was leaked."

## Summary

All five round-2 findings are **genuinely fixed and verified**, not merely claimed:
T1-R2-1 `validate()` now runs over the merged daemon config in `resolve_backends` immediately
after `apply_to` and before `build_store`/`build_embedder`, with a truthful comment; T1-R2-2 the
lease test's second `build()` on the same store/session genuinely discriminates a leaked held
lease (a validate-after-lease regression would return `Conflict`/`Held` and fault the second build),
with sound session-reuse semantics and no `ACTIVE_SESSIONS`/second-writer interference;
T1-R2-3 `as_secs_f64()` is gone from all three Duration messages (`{:?}` → `0ns`); T1-R2-4 the
gc-only-alone case is present so "Each override alone is also rejected" is true; T1-R2-5 the
`resolve_store_only` asymmetry is documented. The core T1 guarantee — `validate()` in `build()`
before lease/load/spawn, all interval cadences plus `gc_interval` covered — holds unchanged, and
the `resolve.rs` reorder neither breaks config construction nor changes the error type. `cargo
check` and all four targeted tests pass; no regression found. One residual nit (T1-R3-N1) about an
inline assertion comment's proof attribution; the test itself is sound. The R1-2 deferral to T2
remains the single known, documented non-defect. Verdict: **APPROVE**.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": ["T1-R3-N1"] }, "summary": "Round-3 re-review of the remediated T1 worktree. All five R2 findings are genuinely fixed and verified in context: (R2-1) validate() now precedes build_store/build_embedder in resolve_backends with a truthful comment and unchanged error type; (R2-2) the lease test's second build on the same store/session/agent with a valid config genuinely discriminates a leaked held lease (validate-after-lease would wedge it via Held/Conflict), with valid session-reuse semantics and no ACTIVE_SESSIONS/second-writer interference; (R2-3) as_secs_f64() is absent from all source, the three Duration messages render 0ns via {:?}; (R2-4) the gc-only-alone case was added so 'Each override alone' is now true; (R2-5) resolve_store_only's deliberate non-validation is documented. Core T1 guarantee intact (validate in build before lease/load/spawn; all cadences covered; gc_interval>=1); cargo check and all four targeted tests pass; the resolve.rs reorder is behavior-preserving except the intended fail-fast precedence. One residual nit: the inline assert comment at memory.rs:3482-3483 over-attributes the 'before the lease' proof to the lone Config-error assertion rather than the second build. R1-2 deferral to T2 remains the single known non-defect. Verdict: APPROVE." }
```
