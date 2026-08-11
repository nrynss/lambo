# Adversarial Review: T2.5 — demote()

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.5 — `demote()` (spec §6.1, §7)
**Scope:** `src/graph/demote.rs`, one `pub mod demote;` line in `src/graph/mod.rs`
**Implementer:** T25Demote (`620cd49`); remediation `dca34ba`
**Reviewer:** ReviewT25Demote (round 1), Review2T25Demote (round 2)
**Gate at close:** `cargo test graph::` = 75 passed / 0 failed, 0 warnings.

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | P3 | Stale comment in mutation-log test claimed the first interaction emits a Temporal edge (it does not — no predecessor) | **Fixed** (`dca34ba`): comment corrected |
| R2 | P2 | Synonym-lookup canonical-key path (demote.rs ~143-146, raw-trimmed `graph.synonym` before normalization) untested — only covered indirectly | **Fixed** (`dca34ba`): new test declares register_user→create_user, demotes "register_user" → key "creat user", control "register user" → "regist user" (no chain/transitivity) |
| R3 | P3 | Handoff claimed UAX #29 surprises "all pinned in tests" but the 'Dr.' abbreviation-split was comment-only | **Fixed** (`dca34ba`): 'Dr. Smith left.' now asserted (["Dr.", "Smith left."]), wording corrected |

## Round 2 — verified clean

Verdict ACCEPT, no findings. All three findings closed and verified; pinned contract
holds (UAX #29 `split_sentence_bounds`, one fresh Observation per non-empty sentence
sharing `chunk_group_id`, raw-trimmed synonym lookup before normalization, no match
step, trim+skip-empties, empty chunk → `Ok(vec![])`, NotFound on missing interaction,
assert_invariants in all 10 tests). Crate-behavior claims verified against
unicode-segmentation 1.13.3 source (SB6 numeric, SB7 acronym, SB8; no SATerm data).

## Notable decisions recorded (handoff log)

- UAX #29 crate behavior pinned by tests: "U.S.A. is big." is ONE sentence (SB7);
  "Dr. Smith left." SPLITS after "Dr." (no locale abbreviation lists); SB6 keeps
  "3.14." whole. `split_sentence_bounds` includes trailing whitespace — trim +
  skip-empties is load-bearing.
- Observations skip the canonical match step (context overflow is new information);
  `created_at = Utc::now()` (pinned signature has no clock); `last_demotion_time`
  stays None (P6's canonical demotion is a different operation).
- HRTB note: `canonical_key(sentence, graph.synonym)` does not compile (impl Fn bound
  is higher-ranked); demote.rs does the raw-trimmed lookup inline, semantically
  identical to the pinned flow.
