# A2 implementation record: Gemini config keys

Branch: `a2-gemini-config` (worktree `/home/nryn/work/lambo-wt-a2`). Records the A2 state
verified by the orchestrator after the implementation agent was cancelled mid-flight for
contaminating the main checkout. All code below is the worktree's own, inspected verbatim,
gates re-run by the orchestrator.

## What changed (src/embed/mod.rs)

`EmbedderConfig` gained four Option fields, serde default None, completing the
`deny_unknown_fields` surface (keys declared, not merely read):

| Field | Type | Doc |
| --- | --- | --- |
| `gemini_project` | `Option<String>` | GCP project id (Vertex caller project) |
| `gemini_location` | `Option<String>` | Vertex region, e.g. `us-central1` |
| `gemini_model` | `Option<String>` | Vertex model id; default `gemini-embedding-001` applied by the A3 adapter when None |
| `gemini_credentials` | `Option<PathBuf>` | Explicit Google service-account JSON key file path; overrides ADC (A3) |

`impl Default` sets all four to `None`. `impl Deserialize` needs no extra work beyond the
fields being present (the `deny_unknown_fields` derive already rejects misspellings).

`overlay_env` gains four vars, following the existing contract (non-empty env wins over the
base; empty env leaves the base intact):

| Env | Field |
| --- | --- |
| `LAMBO_GEMINI_PROJECT` | `gemini_project` |
| `LAMBO_GEMINI_LOCATION` | `gemini_location` |
| `LAMBO_GEMINI_MODEL` | `gemini_model` |
| `LAMBO_GEMINI_CREDENTIALS` | `gemini_credentials` (String into PathBuf) |

## Credential source decision (written down, per spec A2)

**ADC is the default and the fallback; an explicit `gemini_credentials` path overrides it.**

- Mooshik's hosted service should use a dedicated service-account key rather than gcloud
  user credentials, so the adapter (A3) will consume `gemini_credentials` when present.
- ADC (honored by the google auth stack via `GOOGLE_APPLICATION_CREDENTIALS` or gcloud login)
  keeps local/dev working with zero config when the key is absent.
- Both are declared here; the A3 adapter chooses explicit-key-when-present, else ADC.

## Tests (src/embed/mod.rs tests module)

- Extended `empty_embedder_env_defaults_kind`: removes the four `LAMBO_GEMINI_*` vars and
  asserts all four fields are `None` on a clean env.
- New `gemini_toml_fields_deserialize`: a TOML declaring all four keys deserializes and each
  field is read back (deny_unknown_fields accepts the declared keys).
- New `gemini_toml_misspelled_key_rejected`: `gemini_projct` fails to parse (deny_unknown_fields
  rejects a typo, not silently ignored).
- New `gemini_overlay_env_picks_up_vars`: sets each `LAMBO_GEMINI_*`, asserts the fields pick
  them up, then sets `LAMBO_GEMINI_PROJECT=""` and asserts the base stays intact (empty wins
  nothing). Uses `crate::test_util::env_lock()`.
- New `gemini_overlay_env_base_then_env_precedence` (A2-R1-1 closure): a file base with
  `gemini_project` set, then overlay_env; non-empty env overrides the base, empty env leaves
  the base intact.
- New `gemini_overlay_env_whitespace_is_non_empty` (A2-R1-2 closure): a whitespace-only env
  value is non-empty and wins, matching the untrimmed llama pattern; locks the corner.

All use the same pattern as the pre-existing llama config tests; nothing deleted.

## Gates (orchestrator re-ran in this worktree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features embed-bge -- -D warnings` | pass |
| `cargo test --features embed-bge,embed-fixture` | 931 passed / 0 failed / 3 ignored (1 lib, 2 elsewhere; unrelated to A2) |
| `cargo check --features embed-gemini,embed-bge,embed-fixture` | pass |
| `cargo check --no-default-features --features embed-fixture` | pass |
| `cargo doc --no-deps --document-private-items --features embed-bge,embed-fixture` | no gemini doc warnings |

All six gemini tests pass in isolation (26 pass in `embed::tests`, 0 failed).

