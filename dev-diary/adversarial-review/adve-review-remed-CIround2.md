# Adversarial Review — CI-fix round 2 (remed-CI)

- **Worktree**: `/home/nryn/work/worktrees/remed-CI` (detached HEAD at `d117eae`)
- **Scope**: same 10 modified files as round 1 (uncommitted); the *new* delta since round 1 is the P1 remediation in `src/recall/dispatch.rs` (three `#[cfg(feature = "fixtures")]` attributes + restored `chrono` import) and nothing else.
- **Reviewer**: read-only; every CI command re-run locally under the workflow's global `RUSTFLAGS="-D warnings"` (ci.yml:63-65), including forced clean rebuilds (`cargo clean -p lambo`) of the two feature-matrix jobs that failed in round 1 so no stale artifact could mask a residual warning.
- **Disposition**: **APPROVE**
- **Verdict**: **READY.** CI-R1-1 (P1) is genuinely fixed — both `sqlite-minimal` and `cockroach` matrix rows now compile clean from scratch under `-D warnings`. All nine CI rows re-run green; both baselines (758 default / 842 all-features) hold exactly. No new P1/P2. Round-1's P3 (CI-R1-2) remains a true, non-blocking coverage observation; its entry-point coverage claim was re-verified. One round-1 nit (CI-R1-4) turns out to be mischaracterized — the `.flatten()` is load-bearing, not a leftover.

---

## 1. CI-R1-1 (P1) — genuinely fixed, verified from clean builds

### The fix, as committed in the worktree (`src/recall/dispatch.rs:340-345`)
```rust
#[cfg(feature = "fixtures")]
use std::sync::Arc;

use chrono::{TimeZone, Utc};
#[cfg(feature = "fixtures")]
use parking_lot::RwLock;
```
plus `#[cfg(feature = "fixtures")]` on the test at :643. Exactly the round-1 prescription (gate per the `canon/stage1.rs:129-132` convention).

### Adversarial verification — clean rebuilds, not warm cache
Round 1's failure was reproduced with the remediator's own warm target dir; to make sure a stale artifact could not mask a residual unused import, I ran `cargo clean -p lambo` (removed 12,979 files / 28.3 GiB) and rebuilt both failing jobs from scratch:

```
$ RUSTFLAGS="-D warnings" cargo test --no-default-features --features store-sqlite --no-run
   Compiling lambo v0.1.0 (...)
    Finished `test` profile in 19.01s
   (zero warnings, zero errors)
$ RUSTFLAGS="-D warnings" cargo test --no-default-features --features store-cockroach --no-run
   Compiling lambo v0.1.0 (...)
    Finished `test` profile in 18.34s
   (zero warnings, zero errors)
```

Full runs (matching the ci.yml matrix commands, which are full `cargo test` for both rows):
- **sqlite-minimal**: `478 passed; 0 failed; 0 ignored` (lib) + integration + doc-tests — **0 ignored** confirms the gated test is compiled out, and the module compiles clean with the `fixtures` feature off.
- **cockroach**: `472 passed; 0 failed; 0 ignored` (lib) + integration — clean.

### Completeness of the gate (review item 2)
- `Arc`/`RwLock` appear **nowhere else** in `dispatch.rs` — grep confirms their only uses are at :652 and :662, both inside the gated test body. The gate is complete.
- `chrono::{TimeZone, Utc}` is **correctly left ungated**: `ts()` (:357-359) calls `Utc.timestamp_opt(...)`, which requires the `TimeZone` trait in scope, and `ts()` is a non-gated helper used by `concept()`/`edge()` → `exhibit()` → the non-gated structural tests. Proof by compile: both minimal feature sets build the test module with `TimeZone`/`Utc` ungated and produce zero unused-import errors. Had `TimeZone` needed gating (it does not), the clean rebuilds would have failed.
- Test-local `use crate::daemon::Daemon;` / `use crate::recall::cache::RecallCache;` live **inside** the gated fn body (:648-649), so gating the fn leaves no dangling imports from the test itself.
- No residual unused item anywhere in the module under minimal features: the whole crate's test module compiles clean under `-D warnings` for both matrix feature sets (a clean build of the lib test artifact). All remaining module helpers (`exhibit`, `sid`, `query`, `content_of`, `ts`, `concept`, `edge`) are used by non-gated tests.

## 2. CI-R1-2 (P3) — coverage note still true, still non-blocking

Gating the whole test still drops the only `Daemon::recall` **entry-point** structural-dispatch coverage (index wiring + cache plumbing) in every `--no-default-features` matrix row. Confirmed equivalent `try_structural`-level coverage remains and is non-gated:
- `dependency_question_returns_structural_dependent_by_traversal` (:538) — positive dependency dispatch over `exhibit()`;
- delete-safety dispatch test (:556-557);
- negative cases: non-dependency marker (:531), general query (:575), unknown target (:581);
- `instrumentation_reports_per_hit_arm_contributions` (:620) — exercises the same traversal with instrumentation.

All are plain `#[test]`s with no cfg. The only three `fixtures` gates in the module are :340, :344, :643. Coverage claim holds; non-blocking as before. (Doc correction: round 1's P3 named a test `structural_query_dispatches` that does not exist in this file — see nit CI-R2-3.)

## 3. CI-R1-4 (nit) — mischaracterized in round 1; `.flatten()` is required, not a leftover

`src/canon/gate.rs:148`: `cooldown_until: in_cooldown.then_some(until).flatten()`. `until` is `Option<DateTime<Utc>>`, so `then_some(until)` is `Option<Option<DateTime<Utc>>>` and `.flatten()` is **necessary** to collapse it into the field's `Option<DateTime<Utc>>`. Round 1 called the `.flatten()` "a leftover from the pre-lint `then(|| …)` shape" — it was load-bearing in the `then(|| until)` shape too. The lint fix (`then` → `then_some`) is correct and eager-evaluation-safe (round 1 verified `until` is computed at :137 with no side effects). Clippy is silent (the clippy row passes). The `if in_cooldown { until } else { None }` alternative would be marginally more direct, but the current form is correct, idiomatic, and clippy-clean — **no action**. This clears CI-R1-4 as resolved-by-characterization rather than resolved-by-edit.

## 4. Full CI matrix — all rows re-run green under `RUSTFLAGS="-D warnings"`

| Job row | Command (ci.yml) | Result (this review) |
|---|---|---|
| fmt | `cargo fmt --all -- --check` | PASS |
| clippy | `cargo clippy --all-targets -- -D warnings` | PASS (no warnings) |
| test (check) | `cargo test --all` | PASS — **758 passed** (baseline match), 3 ignored |
| sqlite | `cargo test --features store-sqlite` | PASS — **816 passed** (round-1 match), 1 ignored |
| sqlite-minimal | `cargo test --no-default-features --features store-sqlite` | PASS — 478 lib, **clean rebuild, 0 warnings** |
| cockroach | `cargo test --no-default-features --features store-cockroach` | PASS — 472 lib, **clean rebuild, 0 warnings** |
| minimal | `cargo check --no-default-features` | PASS |
| demo | `cargo check --features demo` | PASS |
| all-features | `cargo test --all-features` | PASS — **842 passed** (baseline match), 10 ignored |

Baseline check (review item 5): 758 (default) and 842 (all-features) match the stated baselines exactly. The ignored counts (3 default / 10 all-features) are the pre-existing `#[ignore]`d live-tier tests: `embed::bge_m3::tests::live_smoke_against_llama_server` (lib) and `report_bge3_cosine_distribution` + `context_embedding_separation` in `tests/live_calibration.rs` — none of which is in the 10-file diff. Round 1 recorded only the lib line ("1 ignored") and under-counted; not a regression (nit CI-R2-4).

## 5. Regression scan of the full 10-file diff (review item 6) — behavior-neutral

Re-read every hunk of the full diff this round (dispatch.rs in full; the previously-elided cli/mod.rs and serve_web.rs hunks included):
- **dispatch.rs**: the three `cfg(feature = "fixtures")` attributes + restored `chrono` import (the P1 fix, section 1); rustfmt reflows (tuple-array line wraps in `exhibit()`, assert reflows); two `iter().any(|c| *c == …)` → `contains(&…)` (identical element-wise semantics, :543/:678); collapsible-if merge in `max_structural_strength` (:239-244) — side-effect-free comparisons, identical short-circuit semantics; merged doc comment (:66-75) is the correct rustfix from round 1 and still reads coherently.
- **gate.rs**: `then(|| until)` → `then_some(until)` (section 3) + a trailing blank-line removal.
- **cli/derive.rs, cli/mod.rs, cli/recall.rs, serve_web.rs, config.rs, daemon/mod.rs, graph/derive.rs, recall/assemble.rs**: all remaining hunks are pure rustfmt reflows (line wraps, one `use super::{x};` → `use super::x;` idiom fix, blank-line removal before a doc comment in serve_web.rs:491, reflowed `match` in cli/mod.rs:710-719, reflowed sort closures and test-helper calls in serve_web.rs). No logic, string, ordering, literal, or control-flow changes anywhere.
- `git status`: exactly the same 10 modified files; nothing else touched.

## 6. Nits (anything new this round)

- **CI-R2-1** — Import placement in the test module (`dispatch.rs:340-345`): the `#[cfg(feature = "fixtures")] use parking_lot::RwLock;` sits between `use chrono::{TimeZone, Utc};` and `use super::*;`, sandwiching the ungated chrono line between the two gated imports. rustfmt cannot reorder across cfg-attributed items, so this is stable under `cargo fmt --check`; it reads slightly better with the two gated imports adjacent (Arc, then RwLock). Cosmetic; optional.
- **CI-R2-2** — Round-1 nit CI-R1-4 was mischaracterized: `gate.rs:148` `.flatten()` is required to collapse `Option<Option<DateTime<Utc>>>` (see section 3), not a leftover. Doc-only correction; no code action. The `then_some` fix itself stands as verified-correct.
- **CI-R2-3** — Round-1 P3 (CI-R1-2) cited test name `structural_query_dispatches`, which does not exist in `dispatch.rs`; the actual equivalent-coverage tests are `dependency_question_returns_structural_dependent_by_traversal` (:538) and the delete-safety dispatch test (:556). Doc-only; the coverage claim itself is true.
- **CI-R2-4** — Round 1's "758 passed, 1 ignored" for `test --all` under-counted ignored: the current run shows 3 ignored (1 lib + 2 in `tests/live_calibration.rs`), all pre-existing `#[ignore]`d live-tier tests outside the diff. Pass counts (758/842) are the operative baseline and match exactly. Record-keeping nit only.
- **CI-R2-5** — Round-1 handoff-drift nit (CI-R1-3: 11 vs 10 files, clippy-fix line refs) is unchanged and remains doc-only; the change set is still 10 files.

---

## Summary

The P1 remediation is exactly what round 1 prescribed and it works: `Arc`/`RwLock` are `#[cfg(feature = "fixtures")]`-gated at `dispatch.rs:340/:344`, the test is gated at :643, and the accidentally-dropped `use chrono::{TimeZone, Utc};` is restored ungated at :343 — where it is genuinely required by the non-gated `ts()` helper (`Utc.timestamp_opt` needs the `TimeZone` trait). Forced **clean rebuilds** of both previously-failing feature-matrix jobs (`sqlite-minimal`, `cockroach`) compile and run with zero warnings/errors under the workflow's `RUSTFLAGS="-D warnings"`, proving no residual unused import or variable anywhere in the test module under the minimal feature sets. All nine CI rows re-run green: fmt, clippy `-D warnings`, `test --all` (758), sqlite (816), sqlite-minimal, cockroach, minimal, demo, all-features (842) — both baselines hold exactly, and the full 10-file diff is behavior-neutral (rustfmt reflows + the verified clippy fixes + the gate change). Round-1's P3 coverage observation still holds (equivalent non-gated `try_structural` coverage at :538/:556/:575/:581/:620; only the `Daemon::recall` entry-point wrapper is dropped in minimal configs) and remains non-blocking; round-1's CI-R1-4 nit is resolved by re-characterization (`.flatten()` is required, not leftover). No new P1/P2; only cosmetic nits. **CI is genuinely green — APPROVE.**

{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [
      "CI-R1-2 (carried, non-blocking): dispatch.rs:643 — gating the whole test still drops the only Daemon::recall entry-point structural-dispatch coverage in --no-default-features matrix rows; equivalent non-gated try_structural coverage confirmed at :538/:556/:575/:581/:620, so it holds and is non-blocking."
    ],
    "nits": [
      "CI-R2-1: dispatch.rs:340-345 — the gated `use parking_lot::RwLock;` sits between the ungated chrono import and `use super::*;`; rustfmt-stable but reads better with the two gated imports adjacent. Cosmetic, optional.",
      "CI-R2-2: gate.rs:148 — round-1 nit CI-R1-4 was mischaracterized: `.flatten()` is required (then_some(until) is Option<Option<DateTime<Utc>>>); not a leftover, no action. The then_some fix itself is verified correct and clippy-clean.",
      "CI-R2-3: round-1 P3 cited a non-existent test name `structural_query_dispatches`; actual equivalent-coverage tests are dependency_question_returns_structural_dependent_by_traversal (:538) and the delete-safety test (:556). Doc-only.",
      "CI-R2-4: round-1 recorded '1 ignored' for test --all; actual is 3 (1 lib + 2 in tests/live_calibration.rs), all pre-existing #[ignore]d live-tier tests outside the diff. Pass baselines 758/842 match exactly; record-keeping only.",
      "CI-R2-5: round-1 CI-R1-3 handoff drift (10 files, not 11) still holds; doc-only."
    ]
  },
  "summary": "P1 CI-R1-1 genuinely fixed and verified from clean rebuilds: Arc/RwLock cfg(fixtures)-gated at dispatch.rs:340/:344, test gated at :643, chrono::{TimeZone, Utc} restored ungated and required by non-gated ts(). sqlite-minimal (478 lib) and cockroach (472 lib) compile and run under RUSTFLAGS=-D warnings with zero warnings after cargo clean -p lambo; no residual unused item under minimal features (incl. TimeZone). All nine CI rows green: fmt, clippy, test --all (758), sqlite (816), sqlite-minimal, cockroach, minimal, demo, all-features (842) — baselines exact, full 10-file diff behavior-neutral. CI-R1-2 coverage note holds, non-blocking; CI-R1-4 re-characterized (.flatten() required, not leftover). Only cosmetic nits; no P1/P2. CI genuinely green — APPROVE."
}
