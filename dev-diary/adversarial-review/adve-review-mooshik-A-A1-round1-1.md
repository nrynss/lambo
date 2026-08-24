# Adversarial review confirm - mooshik A (Gemini embedder), A1 registry wiring, round 1 P3 closure re-confirm

**Reviewer**: independent adversarial reviewer, agent_id `A1P3Confirm`.
**Scope**: confirm that the three P3 findings from round 1
(`dev-diary/adversarial-review/adve-review-mooshik-A-A1-round1.md`) are closed on the
COMMITTED lambo-for-mooshik state, reviewed via committed content only:
`git show aa6a397` (the remediation commit) and `git show HEAD:src/embed/mod.rs`.
The live working tree is intentionally NOT used (a sibling agent edits it concurrently).
**Files touched by this review**: this review file only. No source files were edited.
**Verdict**: **APPROVE** - all three A1 P3 findings are closed on the committed state, each
with committed evidence. No P1, no P2, no new P3.

## Method

1. Read the round-1 review to extract the exact wording and acceptance bar of each P3.
2. Reviewed `git show aa6a397 --stat` and the per-file diff against the three target files.
3. Confirmed the current committed file content via `git show HEAD:src/embed/mod.rs`
   (test body at lines 845-870).
4. Ran the committed gated test to confirm it passes and is not vacuous.

## P3-by-P3 closure

### A1-A1-1 (CLOSED) - feature-on error not behavior-tested

Round-1 finding: the feature-ON arm's exact fail-closed error string compiled but was not
behavior-tested; a wrong-Ok regression would compile and evade the suite.

Committed `aa6a397` adds a `#[cfg(feature = "embed-gemini")]` test
`gemini_feature_on_fail_closed_names_a3` to `src/embed/mod.rs` (HEAD:856-870). The test:

- builds `build_embedder` for `EmbedderKind::Gemini` with the feature on, then asserts the
  result is an `Err`, panicking on `Ok` ("feature-on arm must error, got Ok (silent fallback
  forbidden)"). This catches a wrong-Ok (silent fallback) regression.
- asserts the message contains "not implemented yet (A3)" (locks naming A3) and
  "embed-gemini" (locks naming the feature).

Evidence: `cargo test --features embed-gemini,embed-bge,embed-fixture --lib
gemini_feature_on_fail_closed_names_a3` passes: 1 passed, 0 failed. The committed test
compiles and behaves as specified. A3 (in another worktree) later supersedes this test with
adapter behavior tests; that is expected and does not affect this A1 closure.

### A1-A1-2 (CLOSED) - spec prose says three expected strings, two exist

Round-1 finding: `A-gemini-embedder.md` prose said "update the three 'expected ...' error
strings", but only two such strings exist (empty-kind and unknown-kind).

`git show aa6a397 -- dev-diary/lambo-for-mooshik/A-gemini-embedder.md` shows the line now
reads: "update the two \"expected ...\" error strings (empty-kind and unknown-kind)".
The over-count is corrected to the true count. CLOSED.

### A1-A1-3 (CLOSED) - pluggability config comment omits gemini

Round-1 finding: `dev-diary/notes/level-b-pluggability.md:78` config-list comment listed
`bge_m3 | bedrock | fixture`, omitting `gemini` (and `candle`).

`git show aa6a397 -- dev-diary/notes/level-b-pluggability.md` shows the comment now reads:
`kind = "bge_m3"  # bge_m3 | candle | gemini | bedrock | fixture`. Both `gemini` and
`candle` are now listed. CLOSED.

## Findings

No P1, no P2, no new P3. The remediation commit `aa6a397` closes all three A1 P3 residue
items with committed evidence in the same three files the round-1 review flagged.

A1P3Confirm, 2026-08-24
