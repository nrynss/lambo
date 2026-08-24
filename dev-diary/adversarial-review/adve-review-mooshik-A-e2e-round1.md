# Adversarial review - mooshik A (Gemini embedder) end-to-end composition, round 1

**Reviewer**: independent adversarial reviewer, agent_id `A_E2E_Review`. STRICT READ-ONLY; the
only file written is this one.
**Scope**: workstream A (A1 registry, A2 config keys, A3 Vertex adapter, A4 dim guard) as
integrated on the committed `lambo-for-mooshik` HEAD, and its CI row per
`dev-diary/lambo-for-mooshik/README.md` "What each workstream adds back" and the
`A-gemini-embedder.md` "Done when" checklist. This is the composition gate: each phase already
passed its own review; the question here is whether the whole hangs together and meets the
stated acceptance criteria as committed.
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik` @ `8921eac`, clean (verified
before starting: `git status` = clean, only this review file added afterwards).
**Verdict**: **REQUEST_CHANGES** - one P1 and one P2 completeness finding against the A "Done
when" checklist. All five code-level acceptance criteria hold and every gate passes; what blocks
is that item (e), the embed-gemini CI row, is entirely absent from `.github/workflows/ci.yml`,
and no `#[ignore]`d live Vertex test exists to meet that criterion's second half.

## Method

1. Read the committed integrated tree at HEAD: `git show HEAD:src/embed/mod.rs`,
   `HEAD:src/embed/gemini.rs`, `HEAD:src/resolve.rs`, `HEAD:Cargo.toml`,
   `.github/workflows/ci.yml`.
2. Reviewed against `dev-diary/lambo-for-mooshik/A-gemini-embedder.md` (whole doc incl. "Done
   when") and the CI-row requirement in `dev-diary/lambo-for-mooshik/README.md`.
3. Re-read the five prior phase reviews
   (`adve-review-mooshik-A-{A1-round1/-1,A2-round3,A3-round2,A4-round1}.md`); all APPROVE.
4. Ran the full gate matrix myself at committed HEAD (exact counts below). No source edits, no
   mutations (per the E2E read-only brief); verification is by committed source trace plus
   live gate runs plus the phase reviews' mutation checks, which I accept as authoritative for
   the closed items.

## Part 1 - composition and coherence verification

### 1a. Cargo feature unity and neighbours

`embed-gemini = ["dep:reqwest", "dep:jsonwebtoken"]`, with `jsonwebtoken = { version = "9",
optional = true }` in `[dependencies]`. It is optional and NOT in `default`; it is pulled only
by `embed-gemini`. Verified: the default `clippy` and the `--no-default-features --features
embed-fixture` check build without jsonwebtoken, while `embed-gemini` pulls `jsonwebtoken
v9.3.1` (plus `ring`, `rustls`, etc.) as a real, isolated edge. It does not bloat default
builds.

Neighbour compilation, all on a single `embed-gemini` base, all exit 0:
`embed-gemini,embed-bge,embed-fixture`; `embed-gemini,embed-candle`; `embed-gemini,embed-bedrock`;
`demo,embed-gemini`; `ship,embed-gemini`; `--no-default-features --features embed-gemini`;
`--no-default-features --features embed-gemini,embed-fixture`;
`--no-default-features --features embed-gemini,store-sqlite,store-cockroach`. Feature closure
is intact; nothing falls through.

Note (not a defect): neither `ship` (`store-*,embed-bge,embed-fixture`) nor `demo` includes
`embed-gemini`, so prebuilt binaries and the demo build do not carry the Gemini adapter. The
spec does not require it in `ship`; the plan is for Mooshik. Recorded for completeness, P3.

### 1b. build_embedder coherence across kinds

Three Gemini surfaces are distinct and correct:
- Feature-off: `is_compiled()` is false, so the fail-closed pre-check returns
  `missing_feature(Gemini)` ("rebuild with `--features embed-gemini`"), and the `#[cfg(not(feature
  = "embed-gemini"))]` arm also returns `missing_feature` - no arm falls through.
- Feature-on: `build_gemini_embedder` runs, which applies the A4 dim guard FIRST (naming 768,
  1536, 3072), then resolves credentials, project, location, model, builds the client and token
  source, and constructs a real `GeminiEmbedder`. No network is touched at construction.
- `is_ready()` returns true exactly when `embed-gemini` is compiled (A3 flipped it), and stays
  distinct from `is_compiled()` (e.g. `Bedrock` keeps `is_compiled == cfg!(feature)` but
  `is_ready == false`).

Tested: `gemini_is_ready_requires_feature`, `gemini_fail_closed_without_credentials`,
`gemini_feature_on_builds_adapter_from_credentials`, `gemini_rejects_unsupported_dim`.

### 1c. resolve.rs model stamping

`resolve_backends` matches `embedder_cfg.kind`:
- `Gemini` -> `gemini_identity(embedder)` -> `Some(model)` (`gemini-embedding-001` by default).
- `Candle` -> `candle_identity(embedder)`.
- `_` (bge, fixture, bedrock) -> `llama_model` (empty filtered to `None`).

So a Gemini contract is `kind="gemini" model=Some("gemini-embedding-001") dim=<configured>`.
The chain is short and correct. Downcast identity is exercised by the A3-side test
`gemini_feature_on_builds_adapter_from_credentials` (asserts `Some("gemini-embedding-001")`),
but there is no `resolve_backends`-level test driving a gemini config end to end; the resolve.rs
Gemini stamping is trace-verified only (see P3-A-E2E-3).

### 1d. CON-7 / CON-2 / error classification / normalization in the integrated tree

Re-verified in the committed `gemini.rs`:
- CON-7: `embed` rejects empty/whitespace-only text with `Unavailable` before any token mint or
  request.
- CON-2: no retry anywhere; the token source has no silent retry and `request_embedding` makes
  one attempt.
- Classification: token-endpoint or Vertex connect/transport failure -> `Unavailable`;
  non-2xx status, unparseable body, missing prediction, wrong width, non-finite, zero-norm ->
  `Backend`. `is_transient()` maps `Unavailable`=true, `Backend`=false.
- Normalization: `l2_normalize_in_place` rejects non-finite and zero-norm with `Backend` and
  normalizes in place; `outputDimensionality` is sent from `cfg.dim`, and width mismatch with
  that dim is a `Backend`.

These match the contract that the BGE-M3 adapter already honours, and are locked by the
httpmock-based unit tests.

### 1e. A "Done when" checklist

(a) kind=gemini builds an adapter: HOLDS (`build_gemini_embedder`, tested).
(b) empty / non-finite / zero-norm refused: HOLDS (CON-7 + normalization tests).
(c) dim outside {768,1536,3072} fails at construction naming the three: HOLDS (A4 guard first in
`build_gemini_embedder`; `gemini_rejects_unsupported_dim`).
(d) connect failure -> Unavailable, server rejection -> Backend: HOLDS
(`transport_failure_is_unavailable`, `vertex_http_errors_are_backend`).
(e) embed-gemini matrix row in CI (compile + unit, no network; live Vertex `#[ignore]`d):
**NOT MET**. See findings P1-A-E2E-1 and P2-A-E2E-2.

## Part 2 - findings

### P1-A-E2E-1 (P1) - No embed-gemini row in CI

`.github/workflows/ci.yml` has zero occurrences of `gemini` / `embed-gemini` (grep-confirmed).
The `feature-matrix` job rows are: sqlite, sqlite-minimal, sqlite-vectors, minimal, cockroach,
postgres, candle, demo, ship-fixtures. The `check` job lints/tests default features only.
Because neither `default`, `ship`, nor `demo` includes `embed-gemini`, the Gemini adapter is
NEVER compiled, linted, or unit-tested anywhere in CI. This is the A "Done when" item (e), an
explicit acceptance criterion, and it is absent.

Impact is worse than a silent gap because the adapter is security-sensitive: a new HTTP +
OAuth + RS256 (`jsonwebtoken`) surface whose 20+ unit tests and clippy-cleanliness would not be
enforced on any push. Per the review brief, an absent CI row is a completeness finding. A row
mirroring the existing `candle` row pattern was expected:
`command: cargo test --features embed-gemini,embed-bge,embed-fixture` (or a
`cargo clippy --all-targets --features embed-gemini ... && cargo test --features
embed-gemini,embed-bge,embed-fixture` pair), which the local run confirms is green and fully
offline (httpmock, no network, no API key). Not yet in ci.yml.

### P2-A-E2E-2 (P2) - No `#[ignore]`d live Vertex test exists

The same "Done when" item (e) also requires "Live Vertex calls stay `#[ignore]`d". There is no
`#[ignore]` in `src/embed/gemini.rs` and no live-Vertrex test anywhere (grep-confirmed). Every
Gemini test is httpmock-based and runs in normal CI. This diverges from the established adapter
convention: `bge_m3` has `live_smoke_against_llama_server`, the candle adapter has
`live_weights_load_and_embed_on_cpu`, both `#[ignore]`d. The consequence is twofold: (1) there
is no operator-runnable test to verify the real OAuth exchange and `embedContent` round-trip
against Vertex when credentials ARE available, and (2) "live calls stay ignored" has nothing to
ignore. A `#[ignore] #[tokio::test]` live test (e.g. `live_vertex_embed_round_trip`, gated on
`GOOGLE_APPLICATION_CREDENTIALS`, refusing to silently skip without it) should exist alongside
the CI row.

### P3-A-E2E-3 (P3) - No resolve_backends-level gemini integration test

The Gemini -> `Some("gemini-embedding-001")` stamping in `resolve_backends` is correct by code
trace and covered indirectly via `gemini_identity`, but no test drives `resolve_backends` with a
gemini `EmbedderConfig` to assert the resulting `EmbeddingContract.model` and dim. Candle and
fixture have resolve-level tests; gemini does not. Low risk because the wiring is a two-line
match arm, but it is the one composition seam left trace-verified rather than test-locked.

## Part 3 - gates re-run by me at committed HEAD (exact results)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo clippy --all-targets --features embed-gemini,embed-bge,embed-fixture -- -D warnings` | pass |
| `cargo test --features embed-bge,embed-fixture` | lib: 919 total, 918 passed / 0 failed / 1 ignored; all suites exit 0 |
| `cargo test --features embed-gemini,embed-bge,embed-fixture` | lib: 934 total, 933 passed / 0 failed / 1 ignored; all suites exit 0. 22 tests carry "gemini" in the name |
| `cargo check --features ship,embed-gemini` | pass |
| `cargo check --no-default-features --features embed-fixture` | pass (jsonwebtoken not built) |
| `cargo check --no-default-features --features embed-gemini,embed-fixture` | pass |
| `cargo doc --no-deps --document-private-items --features embed-gemini,embed-bge,embed-fixture` | pass (exit 0); 54 doc warnings, all pre-existing (`mod@`/paren ambiguous links in `mcp::serve` etc.), none introduced by A |

Additional neighbour combos all pass (exit 0): `embed-gemini,embed-candle`;
`embed-gemini,embed-bedrock`; `demo,embed-gemini`; `--no-default-features --features
embed-gemini`; `--no-default-features --features embed-gemini,store-sqlite,store-cockroach`.

Total local lib testimony: with embed-gemini the lib suite grows by 15 tests (933 vs 918
passing) with zero failures, confirming the adapter and its unit tests are real and green
offline.

## Conclusion

The integrated code is correct: every phase-approval holds in the combined tree, all four
code-level "Done when" criteria (a-d) hold, feature unity is intact, `jsonwebtoken` stays
optional, and every gate I ran is green. What blocks APPROVE is the workstream's own acceptance
item (e), which is entirely absent: **no embed-gemini CI row** (P1) and **no `#[ignore]`d live
Vertex test** (P2). These are completeness, not correctness, defects, and both have clear,
low-risk remediations that the local gates prove will be green.

- A_E2E_Review, 2026-08-24
