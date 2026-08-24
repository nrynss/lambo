# Adversarial review — mooshik A4 (Gemini dim guard), round 1

**Reviewer**: independent adversarial reviewer. Read-only; nothing under review was edited
except this file.
**Scope**: the uncommitted A4 change in worktree `/home/nryn/work/lambo-wt-a4` (branch
`a4-gemini-dimguard`): the new `build_gemini_embedder` dim guard, the two updated A3 test
configs, and the new `gemini_rejects_unsupported_dim` test, per spec section A4 of
`dev-diary/lambo-for-mooshik/A-gemini-embedder.md` and the record
`a-run/A4-implementation.md`.
**Verdict**: **APPROVE** — implementation is correct, minimal, and verified. Zero findings.

## What was verified

1. **Guard placement.** `build_gemini_embedder` (`src/embed/mod.rs:441-491`, whole fn
   `#[cfg(feature = "embed-gemini")]`) rejects `dim` outside `{768, 1536, 3072}` at the very
   top (lines 447-452), BEFORE credential resolution (lines 453-465), before
   `load_credentials`/`build_client`/any request construction. An unsupported
   `outputDimensionality` can never be sent.
2. **Message content.** `"gemini-embedding-001 supports dim 768, 1536 or 3072, got {}"` names
   all three supported widths and the offending value, as the spec `Done when` criterion and
   A3's record (`A3-R1-1` deferral) require.
3. **Feature-off path.** The guard exists only inside the `embed-gemini`-gated function, so a
   build without the feature is byte-for-byte unaffected (the registry pre-check / cfg-arm
   behavior for `Gemini` is unchanged). `gemini_fail_closed_without_credentials` still
   asserts the "not compiled" name on the feature-off arm.

## Mutation check (analytic; strict read-only precluded transient source edits)

- **Remove the guard** → `build_embedder` for a bad dim reaches credential resolution, which
  returns the credentials `Unavailable` error (no `gemini_credentials` /
  `GOOGLE_APPLICATION_CREDENTIALS` in the test env). That message names neither 768/1536/3072
  nor satisfies the `!msg.contains("credentials")` assertion → `gemini_rejects_unsupported_dim`
  FAILS.
- **Move the guard after credential resolution** → a bad-dim build that lacks credentials
  errors at the credentials step before reaching the guard → test FAILS (message is the
  credentials error). Even if credentials were present, `load_credentials` on a nonexistent
  path errors first → still FAILS.
- **Wrong dim choices in the test** → `512/1024/2048` are all outside the set, 1024 being the
  crate default and deliberately unsupported; the three-name assertion catches a guard that
  hardcodes a single accepted value.

The test is therefore a genuine pin on both halves of the contract (position-before-creds and
message content). It is not vacuous.

## Updated A3 tests still mean what they did

- `gemini_fail_closed_without_credentials` now uses `dim: 1536` (valid) → passes the guard,
  reaches the missing-credentials error it asserts on the feature-on arm; feature-off arm
  unchanged.
- `gemini_feature_on_builds_adapter_from_credentials` now uses `dim: 1536` → passes the guard,
  still builds a real `GeminiEmbedder` from a service-account key file and asserts the
  `gemini-embedding-001` identity.
- Both are exercised and green in the gemini gate below (22 passed), so with a valid dim the
  guard no longer shadows them. Neither test was weakened; both keep their original assertion.

## Spec `Done when` criterion (A4)

> A dim outside {768, 1536, 3072} fails at construction, naming the three

Met: the guard fires in `build_gemini_embedder` (construction), names the three, and
`gemini_rejects_unsupported_dim` pins it.

## Gates (rerun by me on the pristine worktree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib gemini` | 22 passed / 0 failed |
| `cargo test --features embed-bge,embed-fixture --lib embed::tests` | 26 passed / 0 failed |

All four counts match the A4 record. No gate finding.

## Findings

None (P1/P2/P3 all empty).

— A4Review, 2026-08-24
