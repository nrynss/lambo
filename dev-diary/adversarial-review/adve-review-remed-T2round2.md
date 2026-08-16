# Adversarial Review: Remediation T2 — `[daemon]` means the same thing in every subcommand (worktree `remed-T2`, round 2)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 2 of the review/remediate loop           ║
║  Scope:  Re-review the FULL current T2 diff after the four      ║
║          round-1 findings (T2-R1-1..4) were remediated.         ║
║          Round 1 verdict was APPROVE with 2 P3 + 2 nits.        ║
║  Branch: remed-T2 (worktree /home/nryn/work/worktrees/remed-T2)║
║  Date:   2026-08-17                                            ║
║  Reviewer: T2ReviewR2 (read-only)                              ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 1 P3 / 0 nits.              ║
║          The three real fixes (test, backends() doc, demo note) ║
║          are genuine and correct. One new P3: the R1-3 reworded ║
║          comment introduced a garbled, factually inverted       ║
║          clause ("applies to serve"). Comment-only, non-blocking.║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only. Working tree (detached HEAD `02d6f2d`) carries the whole T2
change **uncommitted** — `git status` shows modified `src/cli/demo.rs`,
`src/cli/mod.rs`, `src/memory.rs`, plus the untracked R1 review. `git diff`
(full, untruncated) is **38 insertions / 0 deletions** across exactly those
three files: the R1-approved `open_writer` fix plus this round's remediation. I
re-read the changed regions in context, re-verified the original T2 semantics
against `src/memory.rs`, and ran the new regression test.

Targeted run — `cargo test --lib open_writer_forwards_resolved_config_daemon_overrides`
(default features: `store-memory` + `embed-fixture` are both in `default`):
`test cli::tests::open_writer_forwards_resolved_config_daemon_overrides ... ok; 1 passed; 0 failed`.
No full suite / formatter / clippy (Main owns final verification).

## Remediation verification (each of the four R1 findings)

### T2-R1-1 (P3) — regression test now pins the behaviour ✅ FIXED, genuine
New test `cli::tests::open_writer_forwards_resolved_config_daemon_overrides`
(`src/cli/mod.rs:624-639`):

```rust
#[tokio::test]
async fn open_writer_forwards_resolved_config_daemon_overrides() {
    let store = Arc::new(MemoryStore::new());
    let mut backends = backends_on(store);
    backends.config.gc_interval = 17;
    let mem = open_writer(backends, "t2-daemon-config", "agent-a")
        .await.expect("open_writer carries the resolved config");
    assert_eq!(mem.config().gc_interval, 17);
    mem.close().await.expect("close releases the writer lease");
}
```

- **Non-vacuous (adversarially confirmed):** `backends_on` (`:222-244`) seeds
  `config: crate::Config::default()` (→ `gc_interval = 10_000`, `config.rs:151`).
  The test overrides to a non-default `17`. `MemoryBuilder` derives `Default`
  (`memory.rs:395-402`), so if `open_writer` ever fell back to the builder's
  default config (i.e. the `.config(config)` forward were removed — the exact
  T1-R1-2 regression this task exists to close), `mem.config().gc_interval`
  would be `10_000` and `assert_eq!(…, 17)` would fail. It passes because
  `open_writer` forwards the resolved `17`. Genuinely pins the regression.
- **Right feature gate / lib target:** lives in `mod tests` gated on
  `#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]`
  (`:111`) — both are default features, and `src/cli/mod.rs` is part of the
  **lib** target, so the test is compiled/run by `cargo test --lib` (verified:
  it ran). Correct gate for the `MemoryStore`/`FixtureEmbedder` fixtures it
  uses. `#[tokio::test]` matches every sibling test.
- **Deterministic / isolated:** builds on a fresh in-RAM `MemoryStore` (no file
  or env leakage), unique session id `"t2-daemon-config"`, and closes the
  writer so the writer lease is released — no cross-test contention. Does not
  depend on wall clock or ordering.
- `open_writer` on a missing session = first use (non-error) on the in-memory
  store; `17` clears `build()`'s `validate()` (positive), so no spurious fail.

### T2-R1-2 (P3) — config-drop invariant now documented at the root ✅ FIXED (doc path, acceptable)
`MemoryBuilder::backends()` doc (`src/memory.rs:453-459`):

> **Config is deliberately NOT applied here.** This method forwards only the
> store/embedder/embedding — `backends.config` is consumed and dropped. A writer
> built from a resolved backend MUST also pass `.config(backends.config.clone())`
> (before or after — the two fields commute), as `open_writer` and
> `serve::build_memory` do; otherwise the `[daemon]` cadence overrides the
> resolver applied are silently lost and the session behaves as
> `Config::default()`.

This is exactly the "record the invariant in the `backends()` doc comment" option
the R1 finding offered, and it is **accurate** (not misleading):
- `backends()` really does consume and drop `backends.config` (`:460-465`
  handles only store/embedder/embedding).
- "the two fields commute" is true (`config()` and `backends()` write disjoint
  builder fields; `build()` uses `self.config` as the merged base — see below).
- The concrete failure mode it warns of (silent fallback to `Config::default()`)
  matches the real T1-R1-2 fallout. No behaviour change was added — verified the
  method body is untouched.

### T2-R1-3 (nit) — `open_writer` comment reworded (mostly) ⚠️ PARTIAL — see P3 T2-R2-1
The recommended sentence is present verbatim
(`src/cli/mod.rs:69-71`): "Every full-resolve CLI writer verb (`derive` /
`record-action` / `reserve` / `release`) opens its one Memory through this site;
`serve` and `demo` use their own builders." This correctly scopes the claim and
removes the R1 over-claim. **However** the surrounding text still contains a
new, garbled, factually inverted clause — this is the one finding I carry
forward as **T2-R2-1 (P3)**.

### T2-R1-4 (nit) — demo's exclusion stated at the site ✅ FIXED, accurate
One-line note added to `build_config()` (`src/cli/demo.rs:880-882`):

> The demo deliberately does not honour a user `[daemon]` — its own compressed
> cadence and scripted clock (see [`script_clock`]) are required for the
> state-machine demonstration (see `config.rs`).

Accurate: `demo`/`canonization_config` set `DEMO_TICK_INTERVAL`,
`DEMO_FLUSH_INTERVAL`, `BUILD_EVAL_INTERVAL`, `DEMO_GC_INTERVAL` and build with
`script_clock()` via its own `open()` helper (not `open_writer`); honouring a
user `gc_interval = 10_000` would break the demo's purpose. Matches
`src/config.rs:235-241`. The **original** doc line of `build_config()` ("Acts
I–III: canonization frozen, GC at its spec default…") is intact
(`demo.rs:877-878`) — it was NOT lost during the remediation.

## Original T2 semantics — re-verified (no regression)
- `build()` base is `let mut config = self.config;` (`memory.rs:559`); named
  setters only override `match_strategy`/`flush_interval`/`scoring_weights` on
  top (all `None` on the `open_writer` path), and `Config::default()` is never
  re-applied. `.config(config)` **before** `.backends(backends)` in `open_writer`
  (`mod.rs:76-81`) therefore yields exactly `backends.config` as the effective
  daemon config — order-independent as documented.
- `let config = backends.config.clone();` is cloned because `backends` is moved
  into `.backends(..)`; `Config: Clone`. Mirrors `serve::build_memory`.
- `serve` (`build_memory`) unchanged — no double-apply; `[daemon]` applied once
  in `resolve_backends` (T1), validated at resolution (`resolve.rs` boundary)
  **and** again in `build()` (`memory.rs:573`, defense in depth). No bypass and
  no degenerate-file resurrection.
- Every production full-resolve writer verb (derive / record-action / reserve /
  release via `open_writer`; serve via `build_memory`) honours `[daemon]`
  identically; readers, `serve-web`, `provision`, and `demo` (own scripted
  config/clock) correctly remain N/A. No new writer-path bypass was introduced
  by this round's diff (which touches only comments/docs + the test).

## Regression scan
Full worktree diff = 3 files, +38/−0. No logic lines changed this round other
than the already-approved `open_writer` forward (which is unchanged from R1).
No callers migrated, no symbols renamed, no behaviour altered. The doc edits
compile cleanly (the `cargo test --lib` compile of the whole crate confirms all
three files). Nothing else touched — no regression surface beyond what is
reviewed above.

## Findings

### P3

#### T2-R2-1 (P3) — `open_writer` comment contains a garbled, factually inverted clause (introduced by the R1-3 reword)
- **Where:** `src/cli/mod.rs:71-73` (inside the rewording that closed T2-R1-3).
- **What:** After the correct enumerating sentence, the comment continues:
  "So a lowered `gc_interval` in lambo.toml applies to **serve** — not just
  `Config::default()` (T1-R1-2: `open_writer` dropped `backends.config`)."
  This is factually backward: `serve` already honoured a lowered `gc_interval`
  **before** T2 (via `build_memory`); the T2 change is that the **CLI writer
  verbs** (`derive` / `record-action` / `reserve` / `release`) now honour it
  too. Saying it "applies to serve" reads as if `serve` were the newly-affected
  verb, and the contrast "— not just `Config::default()`" is likewise garbled (a
  config value isn't a sensible alternative to a verb). There is also an awkward
  mid-sentence break ("…applies to\n // serve — …").
- **Why it matters:** The whole task is that `[daemon]` must mean the same on
  every verb; the comment guards that invariant. An inverted statement in the
  very comment this round was asked to make precise re-introduces the imprecision
  R1 flagged (albeit in a different clause), and a future reader could walk away
  believing `serve` is the only beneficiary. Behaviour is unaffected.
- **Fix:** Reword the trailing clause, e.g. "So a lowered `gc_interval` in
  lambo.toml is now honoured by these writer verbs too, not silently dropped to
  `Config::default()` (T1-R1-2: `open_writer` previously dropped
  `backends.config`)." Non-blocking (comment-only).

### Nits
- None. The single-sentinel `gc_interval = 17` test is sufficient: `open_writer`
  either forwards the whole resolved config object or falls back wholesale, so
  one non-default knob fully distinguishes "resolved" from "default"; pinning a
  second `[daemon]` knob would add nothing and would contradict the R1-approved
  design. The `memory.rs` and `demo.rs` notes are accurate with no residual
  misleading wording.

## Verified-OK (probed, not defects)
- New test genuinely pins the regression (non-vacuous by construction — default
  `10_000` vs sentinel `17`; builder `config` field defaults to
  `Config::default()`), correct gate, lib target, deterministic, isolated
  session; **runs green**.
- `backends()` config-drop invariant documented accurately at the root;
  `config()`/`backends()` commutativity reaffirmed against `build()`.
- `build()` ordering semantics intact; config survives `.backends()`; every
  production writer covered; demo/serve-web/readers correctly N/A; T1 validation
  not bypassed and not double-applied.
- demo note present/accurate and the original `build_config()` doc line retained.
- Full-diff regression scan clean (+38/−0, comment/doc/test only).

## Summary

All three substantive remediations are genuine and correct, not merely claimed:
the new regression test (T2-R1-1) is non-vacuous, correctly feature-gated to the
lib test target, deterministic, and isolated — I ran it and it passes; the
`backends()` doc (T2-R1-2) accurately records the config-drop invariant at the
root via the acceptable documentation path; and the demo note (T2-R1-4) is
accurate with the original doc line intact. The R1-3 comment reword landed the
precise enumerating sentence, but in the same breath introduced a garbled,
factually inverted trailing clause ("a lowered `gc_interval` … applies to
serve") that contradicts the very behaviour the task guarantees — that is
T2-R2-1 (P3), comment-only and non-blocking. The original T2 semantics are
re-confirmed end to end: `build()` uses the passed `.config()` as the merged
base (order-independent, never `Config::default()`), every production writer to
a daemon session honours `[daemon]` identically, and T1's fail-closed validation
is neither bypassed nor double-applied. No P1/P2. The single P3 is an inaccurate
sentence in one comment, so **APPROVE**.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T2-R2-1"], "nits": [] }, "summary": "Round-2 re-review of the full (uncommitted) T2 diff confirms all four R1 findings are genuinely remediated, not just claimed. T2-R1-1: new open_writer_forwards_resolved_config_daemon_overrides test is non-vacuous (sentinel gc_interval=17 vs default 10000; builder derives Default), correctly gated to the lib test target (store-memory+embed-fixture), deterministic and isolated (fresh in-RAM store, unique session, closes the writer lease); it compiles and passes under cargo test --lib, and fails iff open_writer falls back to Config::default(). T2-R1-2: backends() doc now states the config-drop invariant accurately at the root via the acceptable documentation path (forwards store/embedder/embedding only; config consumed; must pass .config(backends.config.clone()); fields commute) — body unchanged. T2-R1-4: demo build_config() note is accurate and the original doc line is intact. T2-R1-3 is the one partial: the precise 'full-resolve CLI writer verb ... serve and demo use their own builders' sentence is present, but the surrounding text introduced a garbled, factually inverted clause (a lowered gc_interval 'applies to serve') that contradicts the guarantee — carried as new P3 T2-R2-1 (comment-only, non-blocking). Original T2 semantics re-verified: build() uses .config() value as merged base (never Config::default()), config survives .backends(), every production writer honours [daemon] identically, T1 validation neither bypassed nor double-applied, full diff is +38/-0 (comments/docs/test only). No P1/P2. APPROVE." }
