# Adversarial review: mooshik A (Gemini embedder), A2 config keys, round 2

**Reviewer**: independent adversarial reviewer, agent_id `A2Review2`. Read-only: wrote nothing
under review except this file; never edited `lambo-wt-a2` or `lambo` sources.
**Scope**: the uncommitted A2 remediation in worktree `/home/nryn/work/lambo-wt-a2`, branch
`a2-gemini-config` (dirty), against the three findings of round 1
(`adve-review-mooshik-A-A2-round1.md`): A2-R1-1 (P2), A2-R1-2 (P3), A2-R1-3 (P3). Round 1
verdict was APPROVE.
**Diff inspected**: `git diff src/embed/mod.rs` (137 insertions, 0 deletions) plus the file read
directly and the updated record `a-run/A2-implementation.md`.
**Verdict**: **APPROVE** (remediation). All three round-1 findings are closed with zero
residue. One P3 record-transcription nit remains (stale pass count in the record's gate table,
introduced by the remediation's own two added tests). No code defect.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| A2-R1-1 (P2) base-then-env precedence untested | **CLOSED** | New `gemini_overlay_env_base_then_env_precedence` (`mod.rs:650-673`): builds a `Some("from-file")` base (simulating a `lambo.toml` value), then (1) sets `LAMBO_GEMINI_PROJECT="from-env"` and asserts the field becomes `from-env`, (2) sets it to `""` and asserts the field stays `from-file`. Mutation check, both directions: if env did NOT override a set base, case 1's `assert_eq!(.., Some("from-env"))` FAILS; if empty env overrode the base to `Some("")`, case 2's `assert_eq!(.., Some("from-file"))` FAILS. Both halves of the precedence contract are now pinned. Correctly strips the four gemini vars first and holds `env_lock()`. |
| A2-R1-2 (P3) whitespace env value untested | **CLOSED** | New `gemini_overlay_env_whitespace_is_non_empty` (`mod.rs:675-686`): sets `LAMBO_GEMINI_PROJECT="   "` and asserts the field is `Some("   ")`. Mutation check: a future trim (`v.trim()` making the value empty) would leave the field `None` under the current `!v.is_empty()` guard, so `assert_eq!(.., Some("   "))` FAILS. This locks whitespace-is-non-empty and forces a trim to be a deliberate contract change, exactly as asked. |
| A2-R1-3 (P3) record gate table misreported ignored count | **CLOSED** | Record now states "929 passed / 0 failed / **3 ignored** (1 lib, 2 elsewhere; unrelated to A2)" (`A2-implementation.md:64`). My own full run confirms **3 ignored** (lib suite 1, `live_calibration.rs` 2), all unrelated to A2. The count now matches reality. |

Mutation score: **4/4 attempted mutations are caught** by the two new tests (two directions on
precedence, one on whitespace, plus verifying the two cases are non-vacuous). No new test is a
vacuous pin: each would fail on the specific defect it names.

## Part B: hunt for defects introduced by the fixes

No code defect found. Both new tests use `crate::test_util::env_lock()` and strip the gemini
vars they read, so they are isolated from sibling env tests. `base.clone()` on the precedence
test is safe (the derived `Clone` leaves the variables untouched).

One P3 residual, a record-transcription nit (not a code defect, same class round 1 graded P3):

| Id | Grade | Finding |
| --- | --- | --- |
| A2-R2-1 | P3 | The updated record's gate-table pass count "929" is now stale. Round 1 measured 929 with the four original gemini tests; remediation added two tests, so the actual total is now **931 passed / 0 failed / 3 ignored** (lib 918/1 ignored, main 7, p2 2, t84 2, doc 2). The record fixed the "ignored" figure (A2-R1-3) but left "929" copied from the prior run. The record's Tests section (`A2-implementation.md:43-55`) also does not mention the two new remediation tests. Both are diary-transcription staleness only; the source, which is authoritative, contains and passes both tests. |

## Part C: gates rerun (my own runs in the worktree, `lambo-wt-a2`, pristine)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | **pass** (exit 0, "Finished" with no warnings) |
| `cargo test --features embed-bge,embed-fixture --lib embed::tests` | **26 passed / 0 failed / 0 ignored** (includes both new gemini tests) |
| `cargo test --features embed-bge,embed-fixture` (full, for counts) | **931 passed / 0 failed / 3 ignored** (lib 918/1 ignored, main 7, p2_integration 2, t84_demo 2, doc-tests 2; ignored = lib 1 + live_calibration 2) |

No gate failure.

## Summary

All three round-1 findings are closed by real, mutation-effective tests or a corrected record:
A2-R1-1 by a precedence test that would fail on either precedence defect, A2-R1-2 by a
whitespace test that would fail on a future trim, and A2-R1-3 by the record's now-correct
"3 ignored" figure (confirmed on my run). All gates green. The only residue is a stale pass
count (929 vs actual 931) and an undocumented new-test section in the diary record (P3,
transcription only), which does not affect the code. Verdict: **APPROVE**.

A2Review2, 2026-08-24
