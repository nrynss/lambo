# A4 implementation record: Gemini dim guard

Branch: `a4-gemini-dimguard` (worktree `/home/nryn/work/lambo-wt-a4`). Implemented by the
orchestrator. A3 (the adapter) is already merged.

## What changed (src/embed/mod.rs)

`build_gemini_embedder` (the config-to-adapter construction site, `#[cfg(feature =
"embed-gemini")]`) now rejects any configured `dim` outside `{768, 1536, 3072}` at the very
top, BEFORE credentials are resolved or any request could be sent:

```
if ![768, 1536, 3072].contains(&cfg.dim) {
    return Err(EmbedError::Unavailable(format!(
        "gemini-embedding-001 supports dim 768, 1536 or 3072, got {}", cfg.dim
    )));
}
```

This is the A4 guard required by spec: gemini-embedding-001 truncates to 768/1536/3072 only,
so an unsupported `outputDimensionality` is never sent (A3 sends `outputDimensionality=dim`).
It also closes A3-R1-1's deferred construction guard exactly where A3's record said it lives.

## Test changes and additions

- New `gemini_rejects_unsupported_dim` (cfg embed-gemini): `512`, `1024`, `2048` all fail at
  construction with a message naming 768, 1536 and 3072, and the message is NOT the
  credentials error (proving the guard fires first).
- Updated the two pre-existing A3 registry tests that used `dim: 1024` (the crate default,
  now correctly unsupported for gemini):
  - `gemini_fail_closed_without_credentials`: now `dim: 1536`, so it passes the guard and
    reaches the missing-credentials error it asserts.
  - `gemini_feature_on_builds_adapter_from_credentials`: now `dim: 1536`, so it passes the
    guard and builds the adapter.

## Gates (orchestrator re-ran in this worktree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib gemini` | 22 passed / 0 failed (was 21; +1 A4 test) |
| `cargo test --features embed-bge,embed-fixture --lib embed::tests` | 26 passed / 0 failed (no regression) |
