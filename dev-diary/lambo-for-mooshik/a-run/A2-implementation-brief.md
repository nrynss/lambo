# A2 implementation brief: Gemini config keys

Worktree: `/home/nryn/work/lambo-wt-a2`, branch `a2-gemini-config`, off `lambo-for-mooshik`.
A1 (EmbedderKind::Gemini registry) is already merged and present.

## Goal
Declare the Gemini config keys on `EmbedderConfig` (which is `#[serde(deny_unknown_fields)]`,
so keys must be declared, not merely read), and extend `overlay_env` with matching `LAMBO_*`
env vars. Mirror the existing `llama_url` / `llama_model` pattern. Adapter still lands in A3;
`is_ready()` stays `false`; `build_embedder` Gemini arm unchanged.

## Fields to add to `EmbedderConfig` (src/embed/mod.rs)
All `Option`-typed, serde default None, mirroring the llama fields:
- `gemini_project: Option<String>` - GCP project id (Vertex caller project).
- `gemini_location: Option<String>` - Vertex region, e.g. `us-central1`.
- `gemini_model: Option<String>` - Vertex model id (default `gemini-embedding-001` applied by
  the A3 adapter when None).
- `gemini_credentials: Option<PathBuf>` - explicit Google service-account JSON key file path.

Also update:
- `impl Default for EmbedderConfig` to add all four as `None`.
- `impl Deserialize`: the `deny_unknown_fields` derive needs no extra work beyond the fields
  being present on the struct. Confirm a TOML with a misspelled key still errors and a TOML
  with `gemini_project = ...` parses.

## Credential decision to WRITE DOWN (in the implementation record)
ADC is the default and the fallback; an explicit `gemini_credentials` path overrides it.
Rationale: Mooshik's hosted service should use a dedicated service-account key rather than
gcloud user credentials; ADC keeps local/dev working with zero config. Both are declared here;
the A3 adapter consumes them. State this explicitly in A2-implementation.md.

## `overlay_env` additions
Non-empty env wins over the base; empty env leaves the base intact (the existing contract).
- `LAMBO_GEMINI_PROJECT` -> `gemini_project`
- `LAMBO_GEMINI_LOCATION` -> `gemini_location`
- `LAMBO_GEMINI_MODEL` -> `gemini_model`
- `LAMBO_GEMINI_CREDENTIALS` -> `gemini_credentials` (parse as PathBuf; invalid path value is
  a user error, mirror how each existing key reports its own parse failure)

## Tests to add / extend (mod.rs tests module)
- Extend `empty_embedder_env_defaults_kind`: it currently removes specific LAMBO_* vars; add
  removal of the four new keys so a clean env still yields all-None gemini fields. Confirm the
  new keys are None by default.
- Add a test: TOML with `gemini_project`, `gemini_location`, `gemini_model`,
  `gemini_credentials` all deserialize (deny_unknown_fields accepts them).
- Add a deny_unknown_fields test: a misspelled gemini key (e.g. `gemini_projct = "p"`) errors.
- Add an overlay_env test: set each `LAMBO_GEMINI_*` and confirm the fields pick them up;
  empty env value leaves base intact. Use `crate::test_util::env_lock()` like the existing test.

## Deliverable
`dev-diary/lambo-for-mooshik/a-run/A2-implementation.md` in THIS worktree: what changed
(file:line), the written-down credential decision, every added/edited test, every gate result.

## Gates (run ALL, report exact counts)
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --all-targets --features embed-bge -- -D warnings`
- `cargo test --features embed-bge,embed-fixture` (report passed/failed/ignored)
- `cargo check --features embed-gemini,embed-bge,embed-fixture`
- `cargo check --no-default-features --features embed-fixture`
- `cargo doc --no-deps --document-private-items --features embed-bge,embed-fixture` (doc gate,
  catches private-item doc rot in the new fields)

## Hard safety rule (VERY important)
Work ONLY in `/home/nryn/work/lambo-wt-a2`. Before every edit, confirm you are in the worktree:
run `pwd` and `git rev-parse --show-toplevel` and verify the top-level path ends in
`lambo-wt-a2`. NEVER edit `/home/nryn/work/lambo` (the main checkout). A previous agent edited
the wrong checkout and contaminated it; do not repeat that.

## Rules
- NEVER commit. Leave the tree dirty; the orchestrator commits.
- No em dashes in any prose, comment, or doc. Use colon, comma, or full stop.
- Never touch `.env`, `models/`, or anything outside the worktree.
