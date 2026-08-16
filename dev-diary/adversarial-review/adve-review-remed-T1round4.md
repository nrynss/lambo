# Adversarial Review: Remediation T1 — Round 4, Final Clearance (branch `remed/T1`)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: APPROVE — Final clearance for integration into main.        ║
║  Scope:  Re-verify the single post-R3 nit fix (T1-R3-N1) and re-     ║
║          sanity the full current diff against R3's reviewed state.   ║
║  Branch: remed/T1 (worktree /home/nryn/work/worktrees/remed-T1)      ║
║  Reviewer: T1ReviewR4 (read-only)                                    ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 0 P3 / 0 nit open.                 ║
║          Worktree is clean and ready to integrate. The single known  ║
║          non-defect is the R1-2 .backends().config drop, deferred    ║
║          to T2 (documented).                                         ║
╚══════════════════════════════════════════════════════════════════════╝
```

## Grounding

Read-only review in the `remed/T1` worktree (branch `remed/T1`). Re-read the round-3 review
(`adve-review-remed-T1round3.md`), the full `git diff` (`src/config.rs` +150, `src/resolve.rs`
+12/-3, `src/memory.rs` +54), and the nit comment region `src/memory.rs:3457-3505`. No source was
edited. Ran the one targeted test:
`cargo test --quiet --lib build_rejects_zero_cadence_before_acquiring_the_lease` → **1 passed, 0
failed** (722 filtered out). Main owns the full suite / formatter / clippy.

## What changed since round-3 APPROVE — and only that

Round-3 left one residual nit, **T1-R3-N1**: the lease test's inline assert comment
(`src/memory.rs:3482-3483`) over-attributed the "no lease leaked" proof to the single `Config`-error
assertion, when in fact the **second build** is what discriminates a leaked lease. The suggested fix
was to rephrase the comment so it attributes the proof to the second build.

- **The only delta is that comment.** Diff deltas vs R3: `config.rs` +150 (unchanged),
  `resolve.rs` +12/-3 (unchanged), `memory.rs` +54 (was +53 in R3). The single added line is the
  reworded comment now spanning three lines (`memory.rs:3482-3484`), up from two — exactly the
  +53 → +54 step. No other hunk in any of the three files differs from R3's reviewed state.
- **Diff reads exactly as R3 reviewed.** `config.rs`: the three `Duration` cadence guards
  (`{:?}` → `0ns`) plus `gc_interval`, and the three tests
  (`cadence_validation_fails_closed`, `daemon_config_zero_cadence_override_rejected` incl. gc-alone,
  `lambo_file_zero_daemon_cadences_fail_validate`). `resolve.rs`: `validate()` moved before
  `build_store`/`build_embedder` in `resolve_backends` (fail-fast precedence) plus the
  `resolve_store_only` asymmetry doc note. `memory.rs`: `config.validate()?` before the lease in
  `MemoryBuilder::build()`, the rewritten lease test, and the unwrapped nit comment.

## The reworded comment is accurate

`src/memory.rs:3482-3484` now reads:

> `// A Config error — not Conflict/Store/Held — is consistent with validate()`
> `// rejecting before the lease; the second build below is what proves no`
> `// lease was leaked.`

This matches round-3's suggested wording and is **accurate**: a lone `Config` error from `build()`
does not by itself separate validate-before-lease from validate-after-lease on a fresh session — a
validate-after-lease regression also returns a `Config` error but leaves a held unrefreshed lease.
The second `build()` on the same store/session/agent (`:3494-3503`) is the true discriminator: under
a leaked-lease regression it would hit `LeaseOutcome::Held` at `acquire_lease` and fault.
Attributing the proof to the second build is factually correct. The top-of-test doc comment
(`:3457-3464`) was already accurate in R3 and remains so. The nit is **closed**.

## Targeted test confirmation

- `build_rejects_zero_cadence_before_acquiring_the_lease` compiles and **passes** (1 passed / 0
  failed). It asserts a `Config` error on `gc_interval: 0`, then that a follow-up valid build on the
  same store/session/agent succeeds (the no-leak discriminator), then `close()`.

## Findings disposition

- **P1 / P2 / P3:** none. All round-1/2/3 findings are closed; the core T1 guarantee (validate
  before lease/load/spawn; all interval cadences plus `gc_interval` fail closed) holds unchanged
  from R3's verified state.
- **Nits:** T1-R3-N1 (comment proof-attribution) — **closed** by this round's rewording. No new nit.
- **Known non-defect (unchanged, documented):** the R1-2 `.backends().config` drop is deferred to
  T2. This remains the single known deferral; it is recorded, not a regression, and part of the
  documented T2 scope.

## Verdict

The worktree is **clean and ready to integrate into `main`**. The only change since round-3 APPROVE
is the sanctioned T1-R3-N1 comment rewording, which is accurate and pins the lease-not-leaked proof
to the second build as intended; the rest of the diff is byte-for-byte the state R3 verified. The
lease test compiles and passes. No P1/P2/P3/nit remains open. The lone known non-defect (R1-2 → T2)
stays documented and deferred.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": [] }, "summary": "Final round-4 clearance. The single post-R3 change is the T1-R3-N1 inline-comment rewording at src/memory.rs:3482-3484, which now correctly attributes the lease-not-leaked proof to the second build; the full diff otherwise matches R3's approved state exactly (config.rs +150, resolve.rs +12/-3, memory.rs +54 with the +1 line being the 3-line reworded comment). The comment is accurate and matches R3's suggested wording. The targeted lease test build_rejects_zero_cadence_before_acquiring_the_lease compiles and passes (1 passed, 0 failed). No P1/P2/P3/nit remains open; the core T1 guarantee (build-time validate before lease/load/spawn, all interval cadences plus gc_interval fail closed) is unchanged from R3's verified state. The R1-2 .backends().config deferral to T2 (documented) is the single known non-defect. Worktree clean; ready to integrate into main." }
```
