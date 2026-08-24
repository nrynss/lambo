# A1 implementation brief: registry wiring for `embed-gemini`

Worktree: `/home/nryn/work/lambo-wt-a1`, branch `a1-gemini-registry`, off `lambo-for-mooshik`.

## Goal
Add `EmbedderKind::Gemini` and its six registry touch points in `src/embed/mod.rs`,
plus the Cargo feature `embed-gemini`. This is registry wiring only. The adapter
(`src/embed/gemini.rs`) lands in A3; `is_ready()` stays `false` until A3.

## Spec (from dev-diary/lambo-for-mooshik/A-gemini-embedder.md §A1)
Mirror the `Bedrock` arms exactly (Bedrock is the fail-closed "feature on, adapter not
implemented" template).

| Touch point | Value |
| --- | --- |
| enum variant | `Gemini`, doc comment "Vertex Gemini embeddings. Feature: `embed-gemini`." |
| `feature_name()` | `"embed-gemini"` |
| `is_compiled()` | `cfg!(feature = "embed-gemini")` |
| `is_ready()` | **`false`** (until A3 flips it to `true`), with a comment "adapter lands in A3" |
| `FromStr` | `"gemini"` (plus any alias you judge right, e.g. `"vertex"`); update the "expected bge_m3 | candle | bedrock | fixture" error strings to include `gemini` |
| `Display` | `"gemini"` |

Cargo feature in `Cargo.toml`: `embed-gemini = ["dep:reqwest"]` (reqwest is already
optional behind `embed-bge`, so this adds no dependency).

`build_embedder`: add the `Gemini` arm mirroring the `Bedrock` arm exactly:
- `#[cfg(feature = "embed-gemini")]` => `Err(EmbedError::Unavailable("embed-gemini is
  enabled but the Gemini embedder is not implemented yet (A3)".into()))`
- `#[cfg(not(feature = "embed-gemini"))]` => `Err(missing_feature(EmbedderKind::Gemini))`

## Tests that must change (extend, never delete)
- `parses_embedder_kind` (mod.rs ~457): add a `"gemini".parse()` == `Gemini` assertion.
- `unknown...` FromStr error-path assertions: confirm the updated "expected ..." string
  still matches whatever is asserted; update assertions if they hardcode the old list.
- `kind_feature_names` (~654): add `assert_eq!(EmbedderKind::Gemini.feature_name(), "embed-gemini")`.
- Add an `is_ready` assertion that `EmbedderKind::Gemini.is_ready()` is `false`.
- Add a fail-closed test mirroring `bedrock_fail_closed_no_silent_fallback` (~632):
  `build_embedder` with `Gemini` => Err whose message contains "embed-gemini" or
  "not compiled", does NOT contain "fixture", and `!EmbedderKind::Gemini.is_ready()`.
- Extend `toml_kind_aliases_match_from_str` for `"gemini"` if you add an alias.

## Deliverable
A `dev-diary/lambo-for-mooshik/a-run/A1-implementation.md` in THIS worktree recording:
what you changed (file:line), every added/edited test, and every gate result you ran.

## Gates (run ALL, report exact counts)
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --all-targets --features embed-bge -- -D warnings`
- `cargo test --features embed-bge,embed-fixture` (report passed/failed/ignored)
- `cargo check --features embed-gemini` (proves the feature-on arm compiles)
- `cargo check --features embed-gemini,embed-bge,embed-fixture`
- `cargo check --no-default-features --features embed-fixture` (proves default-off
  still compiles and the new cfg-gated arms do not break a minimal build)

## Rules
- NEVER commit. Leave the tree dirty; the orchestrator commits.
- No em dashes in any prose, comment, or doc. Use colon, comma, or full stop.
- Never touch `.env`, `models/`, or anything outside the repo.
- Work ONLY in `/home/nryn/work/lambo-wt-a1`.
