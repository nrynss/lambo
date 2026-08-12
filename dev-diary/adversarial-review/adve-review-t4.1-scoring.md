# Adversarial Review: T4.1 — Scoring + daemon task skeleton

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT (after 1 remediation round)                 ║
║  Opened / Closed: 2026-08-12                                     ║
║  RECORD RECONSTRUCTED POST-HOC (XP-2 remediation, 2026-08-12)    ║
╚══════════════════════════════════════════════════════════════════╝
```

> **Reconstruction notice.** No review record was committed at the time. This
> file was rebuilt on 2026-08-12 from the PHASE-4 status line, the commit
> history, and the code itself, as remediation for finding **XP-2** of
> `adve-review-p4-daemon-opus.md` ("eight claimed review events, zero committed
> records"). Everything below is either quoted from an in-repo artifact or
> derived from the shipped code; the sections headed **Not recoverable** say so
> explicitly rather than inventing review prose. Contemporaneous reviewer notes
> are lost.

**Task:** T4.1 — composite scoring (spec §9) + the daemon task skeleton (spec §2.5)
**Scope:** `src/daemon/score.rs` (new, 654 lines), `src/daemon/mod.rs` (+348),
`src/lib.rs` (+3)
**Implementing commit:** `721dfe0` — *"P4 T4.1: scoring + daemon task skeleton
(spec §9/§2.5)"*
**Merged:** `40fdaee` (`task/p4-t4.1-scoring` → `phase/p4-daemon`)
**Status line** (PHASE-4-daemon.md, section *"T4.1 — Scoring"*): *"done
(2026-08-12, reviewed ACCEPT after 1 remediation round; merged 40fdaee)"*
**Board line (`dev-diary/README.md`, commit `d01abeb`):** *"T4.1 done (scoring +
daemon skeleton, spec §9/§2.5, merged 40fdaee); T4.2–T4.6 OPEN"*

## What the review had in front of it

Reconstructed from `721dfe0`'s own commit body (an in-repo artifact) and the
shipped module:

- `score.rs` — the spec §9 formula verbatim:
  `recency·0.25 + frequency·0.20 + session_activity·0.20 + density·0.35 +
  edge_type_bonus + concept_type_modifier`; every dimension clamped to `[0,1]`
  **before** weighting; non-finite dimensions → `0.0`; no centrality (cut).
  The v0.6.0 dimension and bonus tables are recorded in the module docs as the
  T4.1 interpretation.
- `mod.rs` — the epoch-poll loop: first cycle is the warm-up rescore (spec
  §2.5), then rescoring is gated on `Graph::epoch()` changing; `ScoreTable
  { epoch, ranked }` replaced wholesale per rescore; `spawn` twice panics; no
  lock held across an `.await`.
- 13 tests in `score.rs` and 5 loop tests in `mod.rs` at the time of merge
  (counted in `40fdaee`):
  property tests (bounded input ⇒ bounded finite score, strict monotonicity per
  dimension, NaN/Inf handling), exact per-weight assertions for
  `session_activity` / `frequency`, the fixture-ordering test (`user schema` on
  top of `session-rest-api`, formula-driven rather than hardcoded), and
  skeleton cycle / abort / wake coverage and the double-spawn panic guard.

## Verified clean (re-verified 2026-08-12 against the shipped code)

These are properties this reconstruction confirmed directly, not remembered
review claims:

- The formula matches spec §9 term for term, including the two additive terms.
- Per-dimension clamping happens before weighting (`score.rs`), so no dimension
  can push the composite outside its intended range.
- Division-by-zero is guarded in all three normalizers.
- `Daemon::spawn` enforces single-loop with a `compare_exchange` + panic,
  mirroring `FlushTask::spawn`.
- `parking_lot` guards are `!Send`, so `tokio::spawn`'s `Send` bound makes
  "lock held across `.await`" a compile error rather than a review question.

The later P4-tier review (`adve-review-p4-daemon-opus.md`) independently
re-verified the scoring math and the spec constants and recorded them as clean.

## Not recoverable

- **The remediation round's content.** The status line claims one round, but
  `task/p4-t4.1-scoring` carries exactly one commit (`721dfe0`), so the
  pre-remediation state was amended away. What the reviewer raised and what
  changed in response cannot be recovered from the repository. The most likely
  subject, judging by what the shipped module documents at unusual length, is
  the v0.6.0 dimension/bonus table interpretation — but that is inference, not
  evidence, and is recorded here as such.
- **Gate numbers at close.** Not captured anywhere in-repo. The tier-level gate
  after T4.6 was 328 passed / 0 failed (recorded in PHASE-4's exit criteria).

## Findings reopened by the later tier review

The P4 tier review found four issues in T4.1's surface that this per-task round
did not catch. They are dispositioned in
`adve-review-p4-daemon-opus.md`, not here:

| ID | Issue |
|---|---|
| ALGO-10 | Non-finite `ScoringWeights` (public `f64`s, TOML-admissible) produce NaN composites |
| CONC-1 | `rescore` calls `incident_edges` per concept, which was a full edge scan — 186ms read guard at 4k concepts |
| XP-7 | The tick interval had no const, no `Config` field, no default |
| CONC-4 | No panic containment in the loop `mod.rs` owns |

That they were missed is the substance of XP-2: with no record, this round has
no stated scope and no reopen criteria, so there is no way to tell whether these
were out of scope or overlooked.
