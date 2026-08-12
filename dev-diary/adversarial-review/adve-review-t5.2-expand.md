# Adversarial Review: T5.2 — Phase-2 expansion

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT after 1 remediation round                   ║
║  Round 1: REJECT — T52-1 (P1, NEW-1 dead-code gate)              ║
║  Round 2: ACCEPT (remediation b266eee verified)                  ║
║  Opened / Closed: 2026-08-12                                     ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T5.2 — phase-2 expansion (handoff PHASE-5-recall.md T5.2, spec §8)
**Scope:** `src/recall/expand.rs` (new), `src/recall/mod.rs` (+1 additive line)
**Implementing commit:** `b703bb1` — *"T5.2 phase-2 expansion"*
**Remediation commit:** `b266eee` — *"fix(recall): gate load_rest_api_fixture behind fixtures feature (NEW-1 dead-code gate)"*
**Merged:** `33fb935` (`task/p5-t5.2-expand` → `phase/p5-recall`)
**Status line (PHASE-5-recall.md):** *"done (2026-08-12, reviewed ACCEPT after 1 remediation round; merged 33fb935)"*

## Round 1 — REJECT

- **T52-1 (P1)** — `src/recall/expand.rs`: the test helper `load_rest_api_fixture` was not gated behind
  `#[cfg(feature = "fixtures")]` while its only caller was. With fixtures off it becomes dead code; under the
  repo-wide `RUSTFLAGS="-D warnings"` that is a hard error on the CI rows that run
  `cargo test --no-default-features` (`sqlite-minimal`, `cockroach`). Reproduced:
  `cargo check --all-targets --no-default-features --features store-memory` emitted
  `warning: function 'load_rest_api_fixture' is never used`. Identical failure mode to P4's NEW-1.
  *Fix:* gate the helper itself, drop the inner `cfg(not(feature = "fixtures"))` panic branch.

## Remediation (b266eee)

3 insertions / 9 deletions in expand.rs only: `#[cfg(feature = "fixtures")]` on the helper, inner panic
branch removed, body simplified to a direct `fixtures::load_snapshot` call. Caller stays gated.

## Round 2 — ACCEPT (no findings)

- Both no-default checks re-run warning-free: `--features store-memory` and `--features store-sqlite`
  (`--all-targets` under `-D warnings`).
- 9/9 `recall::expand` tests green under default features, including `golden_phase2_membership_passes`.
- Full branch diff limited to expand.rs (+549) and the additive mod.rs line.
- Criteria re-scan: BFS from candidates (depth = levels from candidate set, default 2); edge priority
  `TRAVERSAL_ORDER` = the five spec-§8 types (Dependency/Causal -> Hierarchical -> CoOccurrence -> Semantic),
  Derives/Temporal excluded with golden-pinned rationale; id-asc within type, priority tests invert ids to
  rule out id-accidental ordering; visited-set cycle guard with first-discovery-wins and no re-expansion
  (cycle+diamond and re-expansion tests); `chunk_group_id` sibling force-inclusion as transitive closure,
  id-ascending, siblings not re-expanded, no node in both `ExpandedSet` fields; score discipline (phase-1
  scores carried, others `UNSCORED` placeholder, scoring delegated to T5.3); API pure/lock-safe, zero I/O;
  empty candidates -> default; duplicate candidate ids collapsed.

## Wave-barrier gate note (integrator)

The CI clippy gate additionally caught two lints that `cargo check`-based reviews do not surface:
`clippy::len_without_is_empty` (cache.rs, `RecallCache`) and `clippy::unnecessary_sort_by` (candidates.rs
test). Fixed on the phase branch: `cb9c478` (rustfmt pass, wave-A files — CI fmt gate) and `e1414d5`
(clippy fixes). Full default-tier gates green at the wave barrier: fmt, clippy `-D warnings`,
`cargo test --all` (395 passed, 0 failed), no-default `store-memory`/`store-sqlite` `--all-targets` clean.
