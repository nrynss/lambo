# Adversarial review — mooshik A (Gemini embedder), live-fix round, a3

**Reviewer**: independent adversarial reviewer, agent_id `A3LiveFixReview`. Wrote nothing
under review except this file.
**Scope**: uncommitted working-tree changes to `/home/nryn/work/lambo` (HEAD `90c94db`):
`src/embed/gemini.rs`, `src/embed/mod.rs`, plus records
`dev-diary/lambo-for-mooshik/a-run/A3-live-vertex-verify.md` and `A3-implementation.md`.
The change fixes a live-Vertex defect (request/response envelope) and adds
`authorized_user` ADC support (the service-account-only `jwt-bearer` path becomes a
two-grant `GoogleOAuthTokenSource`).
**Worktree**: `/home/nryn/work/lambo`, modified files confirmed by
`git status --short` before starting: `MM src/embed/gemini.rs`, `M src/embed/mod.rs`,
`M dev-diary/.../A3-implementation.md`, `?? dev-diary/.../A3-live-vertex-verify.md`.
No source was edited by this reviewer; the only file written is this one.
**Verdict**: **APPROVE** — both grant paths and the real Vertex envelope are correct, the
live test passes against real Vertex, and every offline gate passes. Two P3 doc-nits, no
P1/P2.

## Method

1. Read the full diff (`src/embed/gemini.rs`, `src/embed/mod.rs`), then the whole of
   `src/embed/gemini.rs` (901 lines, incl. all tests), the `build_gemini_embedder` wiring
   in `src/embed/mod.rs`, and the live-verify record.
2. Re-ran every gate myself on the pristine tree (Part C).
3. Ran the live test myself with the real Google ADC (Part D).
4. Mutation-checked the request-body pin, the response-envelope parse, and the
   missing-refresh-token rejection by source inspection of the mock/test construction
   (this review is read-only; no source mutation was performed).
5. Hunted for stale references/regressions introduced by the rename.

## Part A — the six verification points

| # | Claim | Verdict | Evidence |
| --- | --- | --- | --- |
| 1 | Request body is the real `:embedContent` shape | **HOLDS** | `request_embedding` builds `json!({"content": {"parts": [{"text": text}]}, "outputDimensionality": self.dim})` (`gemini.rs:414-423`). Real Vertex schema; the old `{"content": {"content": text}}` draft is gone. |
| 2 | Response parses `embedding.values`, width==dim, L2 normalize | **HOLDS** | `EmbedResponse { embedding: Embed { values: Vec<f32> } }` (`:344-351`); `embed` reads `response.embedding.values`, rejects `len() != self.dim` as `Backend`, then `l2_normalize_in_place` (`:489-499`). `l2_normalize_in_place` rejects non-finite and zero-norm (`:454-474`). Top-level `embedding` object matches the real API; `predictions` parsing is removed. |
| 3 | `GoogleCredentials` enum + `load_credentials` dispatch, both grants, token_uri threading, cache expiry | **HOLDS** | Enum has `ServiceAccount { client_email, private_key, project_id, token_uri }` and `AuthorizedUser { client_id, client_secret, refresh_token, quota_project_id, token_uri }` (`:59-73`). `load_credentials` dispatches on JSON `type` (defaulting to `service_account`), validates required fields (`is_empty()` checks) and defaults `token_uri` to Google's (`:119-187`). `GoogleOAuthTokenSource::access_token` picks `jwt-bearer` (grant_type + RS256 assertion via `EncodingKey::from_rsa_pem`) for a service account and `refresh_token` (client_id/secret/refresh_token) for authorized_user; both POST to `creds.token_uri()`, cache until `expires_in - 60s` (floor 1s), `expires_in` default 3600 (`:272-337`). JWT claims set `aud` to the threaded `token_uri` (`:256`). `mint_jwt` refuses service-account-only minting with a clear error (`:238-248`). |
| 4 | Project resolution: `gemini_project` else `creds.project_id()` incl. ADC quota project | **HOLDS** | `build_gemini_embedder`: `cfg.gemini_project.or(creds.project_id())` then error (`mod.rs:467-477`). `GoogleCredentials::project_id()` returns SA `project_id` or authorized-user `quota_project_id` (`:75-86`). Live test resolves the project from the ADC's `quota_project_id` (`nryn-personal`) with no `LAMBO_GEMINI_PROJECT` set. |
| 5 | Offline tests updated to the real envelope; new authorized_user test; body-pinning; renamed `missing_embedding_is_backend` | **HOLDS** | `embeds_and_normalizes` mock returns `{"embedding": {"values": ...}}` and pins the request body via `body_contains("outputDimensionality":768)`, `"parts"`, `"text"` (`:691-715`). New `authorized_user_uses_refresh_token_grant_and_caches` asserts the `refresh_token` form body and single-hit caching (`:671-688`). `missing_embedding_is_backend` (renamed) mocks `{}` and asserts `Backend` (`:779-789`). JWT mint/verify, cache, transport/HTTP error, dim-mismatch, non-finite/zero-norm tests all present and passing. |
| 6 | No regression | **HOLDS** | All gates pass (Part C); full lib suite green with the new feature set. |

## Part B — mutation checks (source-verified, read-only review)

- **Omit `parts`/`text`/`outputDimensionality` from the body → `embeds_and_normalizes`
  fails.** The mock's `when` predicates include `body_contains("\"outputDimensionality\":768")`,
  `body_contains("\"parts\"")`, `body_contains("\"text\"")` (`:699-701`). A body missing any
  of these substrings never matches the mock, so `mock.assert()` at `:714` fails the test.
  The pin is order-independent (`body_contains`) and both the width and the
  `content.parts[].text` shape are covered. Unlike the round that shipped the wrong
  envelope, the offline mock no longer fabricates a permissive body — it asserts the real
  one.
- **Response-envelope regression → parse fails as `Backend`.** `missing_embedding_is_backend`
  mocks `{"embedding": {...}}` absent entirely (`{}`), which makes
  `serde_json::from_value` fail on the missing `embedding` field → `Backend`
  (`:779-789`). A return to the old `predictions[].embeddings.values` shape would likewise
  fail (no top-level `embedding`). `rejects_dimension_mismatch` (`:792-804`) guards
  width; `malformed_body_is_backend` (`:767-776`) guards non-JSON. All envelope failure
  modes are `Backend`, not silently swallowed.
- **Authorized-user ADC missing `refresh_token` → rejected.** `AuthorizedUserJson` declares
  `refresh_token: String` as a required field (no `#[serde(default)]`, `:112`), so a JSON
  payload omitting it fails `serde_json::from_value` → `Unavailable` "malformed
  authorized-user credentials"; a present-but-empty value is caught by the
  `au.refresh_token.is_empty()` check (`:165-170`). Same for `client_id`/`client_secret`.
  No authorized-user credential can reach the token exchange without all three fields.

## Part C — gates rerun (my own runs, pristine tree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | **pass** |
| `cargo clippy --all-targets -- -D warnings` | **pass** |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib gemini` | **24 passed / 0 failed / 1 ignored** (the live test) |
| `cargo test --features embed-bge,embed-fixture --lib` | **918 passed / 0 failed / 1 ignored** (no regression) |

## Part D — LIVE test against real Vertex

```
GOOGLE_APPLICATION_CREDENTIALS=/home/nryn/.config/gcloud/application_default_credentials.json \
  cargo test --features embed-gemini --lib \
  embed::gemini::tests::gemini_live_embeds_against_vertex -- --ignored --nocapture
```
**PASSED** (`1 passed; 0 failed`). The ADC file was confirmed to be `type= authorized_user`
with `refresh_token` present and `quota_project_id = nryn-personal`, so the run exercised
the real OAuth `refresh_token` grant, resolved the project from the quota project, sent
`outputDimensionality=1536`, received a real 1536-dim Vertex embedding, and asserted the
L2 norm ≈ 1.0. The test binary is a fresh process whose token cache is in-memory
(`Instant`-based, `cached` starts as `None`), so the OAuth exchange and the
`embedContent` POST were genuinely live HTTP to `oauth2.googleapis.com` and
`us-central1-aiplatform.googleapis.com`, not served from any cache. Wall time ~1.8 s
(real network round-trip).

## Findings

- **A3-LF-P3-1 (doc)**: `gemini.rs:26` ("missing prediction/values") and `gemini.rs:405`
  ("parse a single prediction's values") still use the old `predictions` vocabulary now
  that the envelope is a top-level `embedding` object. The historical reference at
  `:341` is correctly labeled as the old draft; the two live-descriptive uses are stale
  wording. Cosmetic, non-blocking.
- No P1 or P2 findings. No stale references to the removed `ServiceAccountTokenSource`,
  `ServiceAccountCredentials`, or `missing_prediction` remain anywhere in `src` (grep
  clean). No new dependencies were required (`Cargo.toml` unchanged), so the authorized_user
  grant reuses the existing `reqwest` `form`/`json` and `serde_json` machinery.

## Operator-leg note (not a finding)

The local live run proves the authorized-user grant end-to-end; a real
service-account-key live run and a CI/hosted runner are still operator-provided items (the
key is not in CI). The service-account path is exercised offline by the JWT mint/verify and
the cached `jwt-bearer` exchange tests (`mints_and_verifies_service_account_jwt`,
`exchanges_jwt_for_access_token_and_caches`), which is honest coverage for a path with no
available key here.

— A3LiveFixReview, 2026-08-24
