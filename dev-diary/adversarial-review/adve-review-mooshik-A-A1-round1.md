# Adversarial review - mooshik A (Gemini embedder), A1 registry wiring, round 1

**Reviewer**: independent adversarial reviewer, agent_id `A1Review`. Wrote nothing under
review except this file.
**Scope**: the uncommitted A1 implementation in worktree `/home/nryn/work/lambo-wt-a1`,
branch `a1-gemini-registry` (dirty tree, NOT committed), reviewed against the brief
`dev-diary/lambo-for-mooshik/a-run/A1-implementation-brief.md`, spec section A1 of
`dev-diary/lambo-for-mooshik/A-gemini-embedder.md`, and the implementation record
`dev-diary/lambo-for-mooshik/a-run/A1-implementation.md`.
**Diff inspected**: `Cargo.toml` (+2) and `src/embed/mod.rs` (+66) only, plus the untracked
`dev-diary/lambo-for-mooshik/a-run/` record. Nothing else touched.
**Verdict**: **APPROVE** - all six registry touch points, the fail-closed Gemini arm, both
error strings, the feature, and the tests are correct. Findings are all P3, non-blocking,
for traceability only.

## Method

1. Read the brief, spec section A1, and the implementation record, then the full diff.
2. Read the full registry (`src/embed/mod.rs:139-238`), `missing_feature`
   (`:370-376`), `build_embedder` (`:390-472`), and the whole test module
   (`:474-728`).
3. Cross-checked the six touch points against baseline (`git show HEAD:...`).
4. Mutation-checked each claimed regression pin mentally and against the actual
   source path the test exercises.
5. Re-ran all seven gates myself in the worktree on the pristine dirty tree
   (exact counts below).

## Part A - per-acceptance verification

### 1. Six registry touch points

| Touch point | Where | Value | Verdict |
| --- | --- | --- | --- |
| enum variant | `mod.rs:149-150` | `Gemini`, doc "Vertex Gemini embeddings. Feature: `embed-gemini`." | Correct, mirrors Bedrock's naming pattern |
| `feature_name()` | `mod.rs:171` | `"embed-gemini"` | Correct |
| `is_compiled()` | `mod.rs:184` | `cfg!(feature = "embed-gemini")` | Correct |
| `is_ready()` | `mod.rs:196-197` | `false`, comment "Adapter lands in A3; false until then." | Correct |
| `FromStr` | `mod.rs:218` | `"gemini" | "vertex"` | Correct; alias chosen consistent with existing aliases (bedrock->"titan", fixture->"fake", bge_m3->"bge") |
| `Display` | `mod.rs:233` | `"gemini"` | Correct, matches FromStr canonical |

### 2. `is_compiled` vs `is_ready` genuinely distinct

Confirmed. `is_compiled(Gemini)` is `cfg!(feature = "embed-gemini")`; `is_ready(Gemini)`
is literally `false`. The registry design note (`:378-381`) distinguishing the message
pre-check (`is_compiled`) from adapter existence (`is_ready`) is respected, exactly as
Bedrock already does (`is_compiled` = cfg, `is_ready` = false at `:198-199`).

### 3. `build_embedder` Gemini arm fail-closed

`mod.rs:444-456`. Feature-on arm (`#[cfg(feature = "embed-gemini")]`) returns
`Err(EmbedError::Unavailable("embed-gemini is enabled but the Gemini embedder is not
implemented yet (A3)"))`. Feature-off arm (`#[cfg(not(feature = "embed-gemini"))]`)
returns `Err(missing_feature(EmbedderKind::Gemini))`, whose message renders
`--features embed-gemini` (`:370-376`). No `Ok(...)` path exists, so no silent fallback.
The `#[cfg(feature = "embed-gemini")]` arm, when active, is also reachable only after the
`is_compiled` pre-check passes, so it never names an uncompiled kind. No fixture leak:
neither arm's message contains "fixture".

One nuance worth stating plainly: when the feature is OFF (all test gates), `build_embedder`
returns `missing_feature` via the `is_compiled` pre-check at `:395-397`, BEFORE reaching
the `cfg(not(...))` match arm. So the `cfg(not(...))` arm is dead in feature-off builds
and the cfg-on arm is dead in feature-on check builds. This mirrors Bedrock exactly; the
behavior each test locks is the one actually reachable under that test's feature set.

### 4. FromStr expected-strings consistent in both errors

Both the empty-kind error (`:211`) and the unknown-kind error (`:222`) were updated to
`bge_m3 | candle | gemini | bedrock | fixture`, identical token order. Baseline (`git show
HEAD`) had exactly these two strings; grep over the whole repo confirms no test or doc
hardcodes the old `bge_m3 | candle | bedrock | fixture` string anywhere. (Spec prose says
"three expected strings"; only two exist in the code. Not a defect, both existing ones
were updated. See P3-A1-2.)

### 5. Cargo feature additive

`embed-gemini = ["dep:reqwest"]` at `Cargo.toml:124`. `reqwest` is `optional = true` at
`:77` and already pulled by `embed-bge = ["dep:reqwest"]`, so this adds no new dependency
and no feature overrides or disables anything. Additive, correct.

### 6. Tests extended, not deleted, each locks its claimed defect

No test was deleted (diff adds `gemini_is_ready_false_until_a3` and
`gemini_fail_closed_no_silent_fallback`; `parses_embedder_kind`,
`toml_kind_aliases_match_from_str`, and `kind_feature_names` were extended in place;
`bedrock_fail_closed_no_silent_fallback` retained untouched).

- `parses_embedder_kind` (`:496-503`): `"gemini"` and `"  vertex  "` parse to `Gemini`.
  Mutation check: if `FromStr` dropped `"gemini"` or `"vertex"`, these `.unwrap()`s panic.
  Locks the canonical name AND the trim-and-lowercase alias handling.
- `toml_kind_aliases_match_from_str` (`:586-587`): `kind = "vertex"` resolves to `Gemini`
  through serde. Mutation check: removing the alias makes this fail. Locks the vertex
  alias at the deserialization layer, independent of the direct-parse test.
- `kind_feature_names` (`:719`): `assert_eq!(EmbedderKind::Gemini.feature_name(),
  "embed-gemini")`. Mutation check: changing the returned string fails it. Locks
  `feature_name`.
- `gemini_is_ready_false_until_a3` (`:685-691`): asserts `!Gemini.is_ready()`. Non-gated,
  always runs. Mutation check: flipping `is_ready(Gemini)` to `true` before A3 fails it.
  Locks the "false until A3" invariant; A3 must update this test when the adapter lands,
  as the spec requires.
- `gemini_fail_closed_no_silent_fallback` (`:693-712`): `build_embedder(Gemini)` returns
  an `Err` whose message contains "embed-gemini" or "not compiled", does NOT contain
  "fixture", and `!Gemini.is_ready()`. Non-gated. Under the test gate (feature off)
  `build_embedder` yields `missing_feature`, whose message contains "embed-gemini"
  (`:374` via `feature_name()`), so the assertion matches. Mutation checks: (a) if
  `build_embedder` returned `Ok` for Gemini (silent fallback), the `let Err(err) = r else
  { panic! }` fails; (b) if messaging leaked the Fixture embedder, the "fixture" assertion
  fails. Locks fail-closed and no-fixture-leak on the reachable off-arm.

Mutation-score summary: every claimed pin is non-vacuous. The only untested surface is the
feature-ON arm's exact error string, which is compile-checked by `cargo check --features
embed-gemini` but not behavior-test-locked (matching the brief's stated bar and Bedrock's
existing practice). See P3-A1-1.

## Part B - hunt for defects introduced by the fix

No new blocking findings. Attack vectors examined:

- **Variant exhaustiveness**: `match` over `EmbedderKind` in `feature_name`,
  `is_compiled`, `is_ready`, `FromStr`, `Display`, and all `build_embedder` arms force the
  compiler to handle `Gemini`; `cargo clippy --all-targets -- -D warnings` passing (re-run
  below) confirms every arm is total and no dead-code warning fires.
- **Serde round-trip**: `#[serde(rename_all = "snake_case")]` serializes `Gemini` as
  `"gemini"`, matching `Display` and `FromStr` canonical, so config written as `gemini`
  round-trips (no deserialization asymmetry like the `Bedrock` "titan" alias which is
  accepted but never emitted). Consistent.
- **Variant ordering**: `Gemini` inserted before `Bedrock` uniformly in the enum,
  `feature_name`, `is_compiled`, `is_ready`, and Display; the error-string token list
  `bge_m3 | candle | gemini | bedrock | fixture` matches Display order. No disjoint ordering.
- **Feature additivity under combination**: `cargo check --features
  embed-gemini,embed-bge,embed-fixture` (re-run below) passes, so the new arm coexists
  with the default set.
- **Build_embedder reachability**: the pre-check at `:395` gates the on-arm on
  `is_compiled`, so the on-arm never fires while uncompiled; the off-arm is dead under
  feature-on, never reachable, so no contradictory dual path. Mirrors Bedrock.

## Gates (re-run by me in the worktree, exact results)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass, exit 0 |
| `cargo clippy --all-targets -- -D warnings` | pass, exit 0, no warnings |
| `cargo clippy --all-targets --features embed-bge -- -D warnings` | pass, exit 0, no warnings |
| `cargo test --features embed-bge,embed-fixture` | 926 passed, 0 failed, 3 ignored |
| `cargo check --features embed-gemini` | pass, exit 0 |
| `cargo check --features embed-gemini,embed-bge,embed-fixture` | pass, exit 0 |
| `cargo check --no-default-features --features embed-fixture` | pass, exit 0 |

All seven gates match the implementation record's reported results exactly (926/0/3 tests;
all checks and lints clean).

## Findings

No P1, no P2. Non-blocking P3 items for traceability:

- **A1-A1-1 (P3)**: The feature-ON arm's exact error string
  (`embed-gemini is enabled but the Gemini embedder is not implemented yet (A3)`) is proven
  to COMPILE by `cargo check --features embed-gemini` but is not behavior-tested, because
  every test gate runs `embed-gemini` OFF. A future regression that made the on-arm return
  `Ok(...)` with a wrong embedder would compile and evade the suite. This mirrors Bedrock's
  existing on-arm blind spot and the brief's explicit bar, so it is not blocking; flagging so
  A3 lands a feature-on test alongside the adapter.
- **A1-A1-2 (P3)**: Spec prose ("update the three 'expected …' error strings") says three;
  only two error strings exist in the code (empty-kind and unknown-kind), both updated.
  Informational: the spec over-counts; no third string was missed.
- **A1-A1-3 (P3)**: `dev-diary/notes/level-b-pluggability.md:78` comment lists
  `kind = "bge_m3" # bge_m3 | bedrock | fixture` and omits `gemini` (and `candle`, already
  omitted before A1). This is a stale config-list comment, not an error-string assertion,
  and predates A1; it is also the doc referenced by the `missing_feature` message. Out of the
  brief's string-update scope; noted for a later docs pass, not a change request.

No source file was changed by this review.

A1Review, 2026-08-24
