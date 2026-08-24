# A1 implementation: registry wiring for `embed-gemini`

Worktree: `/home/nryn/work/lambo-wt-a1`, branch `a1-gemini-registry`.

Phase A1 adds `EmbedderKind::Gemini` and its six registry touch points in
`src/embed/mod.rs`, plus the `embed-gemini` Cargo feature. This is registry
wiring only: the adapter (`src/embed/gemini.rs`) lands in A3, so `is_ready()`
stays `false`. The `Bedrock` arms were mirrored exactly; Bedrock is the
fail-closed "feature on, adapter not implemented" template.

## Changes

### `Cargo.toml`
- Line 123-124: added the feature
  `embed-gemini = ["dep:reqwest"]` (reqwest already optional behind `embed-bge`,
  so no new dependency) with the comment "A1: Vertex Gemini embedder (adapter
  lands in A3).".

### `src/embed/mod.rs`
- Line 149-150: added the `Gemini` enum variant with doc comment "Vertex Gemini
  embeddings. Feature: `embed-gemini`."
- Line 171: `feature_name()` returns `"embed-gemini"`.
- Line 184: `is_compiled()` returns `cfg!(feature = "embed-gemini")`.
- Line 196-197: `is_ready()` returns `false`, with the comment "Adapter lands
  in A3; false until then."
- Line 211: updated the empty-kind error string to
  "(expected bge_m3 | candle | gemini | bedrock | fixture)".
- Line 218: `FromStr` accepts `"gemini" | "vertex"`.
- Line 222: updated the unknown-kind error string to
  "(expected bge_m3 | candle | gemini | bedrock | fixture)".
- Line 233: `Display` renders `"gemini"`.
- Line 444-455: added the `build_embedder` arm mirroring Bedrock:
  - `#[cfg(feature = "embed-gemini")]` returns
    `Err(EmbedError::Unavailable("embed-gemini is enabled but the Gemini
    embedder is not implemented yet (A3)".into()))`.
  - `#[cfg(not(feature = "embed-gemini"))]` returns
    `Err(missing_feature(EmbedderKind::Gemini))`.

## Tests (extended, never deleted)

- `parses_embedder_kind` (mod.rs ~456): added `"gemini"` and `"  vertex  "` to
  `EmbedderKind::Gemini` assertions (lines 496-502). The existing `"  fake  "`
  and error-path assertions are unchanged.
- `toml_kind_aliases_match_from_str` (~583): added `kind = "vertex"` resolving
  to `EmbedderKind::Gemini` (lines 586-587), exercising the alias through
  serde deserialization.
- `kind_feature_names` (~717): added
  `assert_eq!(EmbedderKind::Gemini.feature_name(), "embed-gemini")` (line 719).
- `gemini_is_ready_false_until_a3` (new, ~685): asserts
  `!EmbedderKind::Gemini.is_ready()`.
- `gemini_fail_closed_no_silent_fallback` (new, ~693): mirrors
  `bedrock_fail_closed_no_silent_fallback`: `build_embedder` with
  `EmbedderKind::Gemini` returns an Err whose message contains "embed-gemini"
  or "not compiled", does not contain "fixture", and
  `!EmbedderKind::Gemini.is_ready()`.

## Gates (all run, exact results)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass, exit 0 (formatting applied first) |
| `cargo clippy --all-targets -- -D warnings` | pass, exit 0, no warnings |
| `cargo clippy --all-targets --features embed-bge -- -D warnings` | pass, exit 0, no warnings |
| `cargo test --features embed-bge,embed-fixture` | 926 passed, 0 failed, 3 ignored |
| `cargo check --features embed-gemini` | pass, exit 0 |
| `cargo check --features embed-gemini,embed-bge,embed-fixture` | pass, exit 0 |
| `cargo check --no-default-features --features embed-fixture` | pass, exit 0 |

Test count detail: the crate unit-test binary ran 913 passed, 1 ignored; the
rest (7 + 2 + 2 + 2) came from the integration and doc-test binaries. The two
new gemini tests both pass when run in isolation
(`gemini_is_ready_false_until_a3`, `gemini_fail_closed_no_silent_fallback`).

The `embed-gemini` feature-on arm compiles, default-off still compiles, and the
new cfg-gated Gemini arms do not break a minimal `embed-fixture` build.

No commit was made. The tree is left dirty for the orchestrator.
