# Adversarial Review: Remediation T2 — final clearance (worktree `remed-T2`, round 3)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — Round 3 final clearance                      ║
║  Scope:  Re-confirm the worktree is clean and integration-ready║
║          after the single R2→R3 change (reword of the R2-1      ║
║          garbled comment clause).                               ║
║  Branch: remed-T2 (worktree /home/nryn/work/worktrees/remed-T2)║
║  Date:   2026-08-17                                            ║
║  Reviewer: T2ReviewR3 (read-only)                              ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 0 P3 / 0 nits.              ║
║  R2-1 comment defect is closed. Clean for integration.          ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only. Working tree (detached HEAD `02d6f2d`) carries the whole T2
change **uncommitted** — `git status` shows modified `src/cli/demo.rs`,
`src/cli/mod.rs`, `src/memory.rs`, plus untracked review docs (R1/R2). `git
diff` (full, untruncated) is **40 insertions / 0 deletions** across exactly the
three files. I re-read the `open_writer` comment block end to end, compared it
against the R2 reviewer's suggested fix, re-scanned the full diff, and ran the
regression test. Skipped full suite / formatter / clippy per assignment.

Targeted run — `cargo test --lib open_writer_forwards_resolved_config_daemon_overrides`:
`test cli::tests::open_writer_forwards_resolved_config_daemon_overrides ... ok; 1 passed; 0 failed`.

## R2→R3 delta — the single reword (verify #1)

The only change since Round-2's APPROVE is the trailing clause of the
`open_writer` doc comment (`src/cli/mod.rs:68-76`). Compared to the R2
finding's quoted garbled clause ("So a lowered `gc_interval` in lambo.toml
applies to **serve** — not just `Config::default()`…"), the current text now
reads (verbatim, `:71-76`):

> So a lowered `gc_interval` in lambo.toml is now honoured by these writer
> verbs too, not silently dropped to `Config::default()` (T1-R1-2: `open_writer`
> previously dropped `backends.config`). Resolution already validated the
> cadence (T1's `resolve_backends`), so this cannot resurrect a degenerate
> file.

- **Matches the reviewer's suggested fix:** the R2 doc proposed exactly "…now
  honoured by these writer verbs too, not silently dropped to `Config::default()`
  (T1-R1-2: `open_writer` previously dropped `backends.config`)." This is present
  verbatim. The inverted "applies to serve" fallacy is gone; the leading clause
  ("Every full-resolve CLI writer verb (`derive` / `record-action` / `reserve` /
  `release`) opens its one Memory through this site; `serve` and `demo` use their
  own builders") is intact and still accurate.
- **Grammatically clean:** read the whole block end to end — no duplicate lines,
  no leftover sentence fragments, no awkward mid-sentence line breaks. It reads
  correctly top to bottom and states the precise, factually-correct invariant
  (CLI writer verbs now honour `[daemon]`; `serve`/`demo` use their own builders;
  resolution already validated, so no degenerate output can resurface).

## Full-diff sanity (verify #2)

`git diff` = exactly three files, comment/docs/test only, no logic regression:

- `src/cli/mod.rs` — (a) the R2-approved `open_writer` forward: `let config =
  backends.config.clone();` + `.config(config)` before `.backends(backends)`,
  with the reworded explanatory comment; (b) the new regression test
  `open_writer_forwards_resolved_config_daemon_overrides`. No behaviour change
  beyond the already-approved forward.
- `src/memory.rs` — `backends()` doc note (config-drop invariant) only; method
  body untouched.
- `src/cli/demo.rs` — `build_config()` doc note only; the original doc line
  ("Acts I–III…") is retained.

+40/−0, nothing else touched. No unresolved merge conflict markers, no
stray/dead code.

## Regression test + original T2 semantics (verify #3)

- The new test is present (`src/cli/mod.rs:624-639`) exactly as R2 reviewed:
  non-vacuous sentinel `gc_interval = 17` vs `Config::default()`'s `10_000`,
  correct feature gate (`store-memory` + `embed-fixture`, both default → lib
  target), deterministic/isolated (fresh in-RAM store, unique session
  `"t2-daemon-config"`, closes the writer lease). **Ran green.**
- Original T2 semantics hold: `open_writer` forwards the resolved `backends.config`
  into `build()` as the effective daemon config (order-independent, never
  `Config::default()`); every production full-resolve writer verb honours
  `[daemon]`; `serve`/`demo`/readers correctly N/A; T1's `resolve_backends`
  validation not bypassed nor double-applied.

## Findings

- P1: none.
- P2: none.
- P3: none. (T2-R2-1 — the garbled `open_writer` comment clause — is **closed**:
  the reword lands the reviewer's exact suggested text, verbatim, grammatically
  clean.)
- nits: none.

## Summary

Since Round 2's APPROVE the only working-tree change is the reword of the
`open_writer` comment's trailing clause, and it is exactly right: it matches the
reviewer's suggested fix verbatim, is free of duplicate/garbled lines, and reads
correctly top to bottom with the factually-inverted "applies to serve" claim
removed. The full worktree diff remains limited to the three approved files
(+40/−0), comment/docs/test only. The regression test is present and passes, and
the original T2 semantics are unchanged. No P1/P2/P3/nit remains open — the
worktree is clean and ready for integration into main.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": [] }, "summary": "Round-3 final clearance. The sole R2→R3 change is the reword of the open_writer comment clause (src/cli/mod.rs:68-76), which now matches the R2 reviewer's suggested fix verbatim, is grammatically clean with no duplicate/garbled lines, reads correctly top to bottom, and removes the factually-inverted 'applies to serve' clause — closing P3 T2-R2-1. Full worktree diff remains exactly src/cli/mod.rs + src/memory.rs + src/cli/demo.rs (+40/−0), comment/docs/test only with no logic regression; the original open_writer forward is unchanged and T2 semantics hold (build() uses the passed .config() as the effective daemon config, order-independent, never Config::default(); serve/demo/readers correctly N/A; T1 validation neither bypassed nor double-applied). The regression test open_writer_forwards_resolved_config_daemon_overrides is present, non-vacuous, correct-gated, and passes under cargo test --lib. No P1/P2/P3/nit remains. Clean for integration — APPROVE." }
```
