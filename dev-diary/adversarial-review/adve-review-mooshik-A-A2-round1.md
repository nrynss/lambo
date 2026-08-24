# Adversarial review: mooshik A (Gemini embedder), A2 config keys, round 1

**Reviewer**: independent adversarial reviewer, agent_id `A2Review`. Read-only: wrote nothing
under review except this file; never edited `lambo-wt-a2` or `lambo` sources.
**Scope**: the uncommitted A2 change in worktree `/home/nryn/work/lambo-wt-a2`, branch
`a2-gemini-config` (dirty), reviewed against the brief
`a-run/A2-implementation-brief.md`, spec section A2 of `A-gemini-embedder.md`, and the
implementation record `a-run/A2-implementation.md`.
**Diff inspected**: `git diff src/embed/mod.rs` (99 insertions, 0 deletions) plus the file read
directly.
**Verdict**: **APPROVE**. The implementation is correct and complete against the brief; gates
green. Two test-coverage gaps and one record nit are findings; none reflects a code defect.

## Part A: required verifications

| Item | Result |
| --- | --- |
| Four Option fields on `#[serde(deny_unknown_fields)] EmbedderConfig` (`mod.rs:246`) | **OK**; `gemini_project`, `gemini_location`, `gemini_model`, `gemini_credentials` at `mod.rs:280-291`, each `#[serde(default)]` `Option`. |
| `impl Default` all-None | **OK**; `mod.rs:307-310`. |
| `overlay_env` non-empty-wins / empty-leaves-base for all four (`mod.rs:356-375`) | **OK**; each `if let Ok(v) = env::var(..) { if !v.is_empty() { self.x = Some(v) } }`, mirroring the llama pattern. |
| PathBuf conversion for credentials | **OK**; `mod.rs:373` `self.gemini_credentials = Some(v.into())` (String into PathBuf); locked by assertions using `Path::new`. |
| Credential decision written down | **OK**; record lines 33-41: ADC default and fallback, explicit `gemini_credentials` path overrides; field doc `mod.rs:289` agrees. |
| `is_ready()` stays false, `build_embedder` Gemini arm unchanged | **OK**; diff touches only struct fields, Default, overlay_env, and tests; `gemini_is_ready_false_until_a3` still holds. |
| No em dash in any new prose or comment | **OK**; all four field doc comments and added test comments use colon, comma, or full stop. |

## Part B: per-requirement test verification (mutation analysis)

Each test below was mentally mutation-checked: would it fail on the defect it names?

- **`empty_embedder_env_defaults_kind` (extended, `mod.rs:560-588`)** removes the four
  `LAMBO_GEMINI_*` vars and asserts all four fields are `None`. Would FAIL if `Default` set any
  gemini field to `Some`, or if a leaked ambient `LAMBO_GEMINI_*` were picked up (var is removed
  first). Also asserts `from_env() == default().overlay_env()`. **LOCKS: clean env gives all-None.**
- **`gemini_toml_fields_deserialize` (new, `mod.rs:595-614`)** a TOML declaring all four keys
  deserializes and each field is read back, including `gemini_credentials` as `Path`. Would FAIL
  if `deny_unknown_fields` rejected a declared key, or if any field were misnamed or mistyped.
  **LOCKS: declared keys accepted and round-tripped.**
- **`gemini_toml_misspelled_key_rejected` (new, `mod.rs:617-624`)** `gemini_projct = "p"`
  fails to parse. Would FAIL if `deny_unknown_fields` were not enforced or the derive were
  dropped. **LOCKS: misspelled key rejected, not silently ignored.** Reinforced by the
  pre-existing `unknown_toml_field_rejected` (`mod.rs:590-593`).
- **`gemini_overlay_env_picks_up_vars` (new, `mod.rs:627-648`)** sets all four vars and
  asserts each is picked up (including credentials PathBuf), then sets `LAMBO_GEMINI_PROJECT=""`
  and asserts it stays `None` while the other three remain. Would FAIL if any var were not
  mapped, or if the empty string overrode the (None) base. **LOCKS: non-empty env picked up;
  empty env does not set Some("") on a None base.** Uses `crate::test_util::env_lock()` (verified
  present at `src/test_util.rs:16`).

**Rejection paths asked for**: empty-value handling is covered (empty string leaves base
intact); unknown-key rejection is covered (misspelled gemini key and generic `knd` both err).
Both requested paths are covered.

## Part C: findings

| Id | Grade | Finding |
| --- | --- | --- |
| A2-R1-1 | P2 | **base-then-env precedence is not tested for the four keys.** Every overlay assertion builds the base from `Self::default()` (an all-None base). Nothing constructs a base with a TOML or file value and then overlays env, so the two halves "non-empty env **overrides a set base**" and "empty env **leaves a set base intact**" are untested for gemini (and, grepping `overlay_env` and `from_env`, for any key repo-wide). Implementation is correct (identical to the llama pattern) and the revealed failure mode is exotic, but the brief's stated contract invites the test. Suggested strengthening: `EmbedderConfig { gemini_project: Some("base".into()), ..Default::default() }.overlay_env()` with `LAMBO_GEMINI_PROJECT` set non-empty (assert override) and set empty (assert base survives). Not blocking: code is correct; remediation is a test-only add. |
| A2-R1-2 | P3 | **No whitespace env-value test.** Empty string is tested (`gemini_overlay_env_picks_up_vars`, line 644); a whitespace-only value (e.g. `"  "`) is not. Current contract treats any non-empty value as winning, so `"  "` flows into the field as-is, consistent with the untrimmed llama pattern (not a divergence, not a defect). A test pinning this would tighten the untested corner. |
| A2-R1-3 | P3 | **Implementation-record gate table under-reports ignored tests.** Record claims `cargo test --features embed-bge,embed-fixture` gives "929 passed / 0 failed / 0 ignored". My own run gives **929 passed / 0 failed / 3 ignored** (lib suite 916 passed / 1 ignored; `live_calibration.rs` 2 ignored). Pass count matches; the three ignored are unrelated to A2 (no gemini test is `#[ignore]`d). Record transcription nit only. |

## Part D: gates rerun (my own runs in the worktree, `lambo-wt-a2`, pristine)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | **pass** (exit 0) |
| `cargo test --features embed-bge,embed-fixture` | **929 passed / 0 failed / 3 ignored** across all suites (lib 916/1, main 7, p2_integration 2, t84_demo 2, doc-tests 2; ignored = lib 1 + live_calibration 2) |
| `cargo check --features embed-gemini,embed-bge,embed-fixture` | **pass** (exit 0) |
| `cargo check --no-default-features --features embed-fixture` | **pass** (exit 0) |
| gemini-filtered test run (`cargo test ... gemini`) | **5 passed / 0 failed**; the four A2 tests plus `gemini_is_ready_false_until_a3` |

No gate failure.

## Summary

Implementation is correct, complete, and consistent with the established llama pattern and the
A2 brief. All four required verifications hold. The only substantive gap (A2-R1-1) is an
untested half of the precedence contract; the code implements it correctly and the fix is a
test-only add in remediation. Verdict: **APPROVE**.

A2Review, 2026-08-24
