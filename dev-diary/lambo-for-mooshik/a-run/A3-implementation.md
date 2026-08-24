# A3 implementation record: the Gemini (Vertex) embedder adapter

Branch: `a3-gemini-adapter` (worktree `/home/nryn/work/lambo-wt-a3`). Implements the A3
specified in `A3-implementation-brief.md`: a real Vertex `gemini-embedding-001` adapter that
mints a service-account OAuth token (RS256 JWT -> token exchange), calls Vertex
`embedContent`, L2-normalizes, and honours every inherited `Embedder` contract. This is my
own record of the work; all gates were re-run here and counts are exact.

## What changed

### Cargo.toml
- `jsonwebtoken = { version = "9", optional = true }` (line 86). Added as an optional dep;
  `jsonwebtoken::EncodingKey::from_rsa_pem` signs RS256 and parses the service-account PEM
  directly, so no other crypto dependency is needed.
- `embed-gemini = ["dep:reqwest", "dep:jsonwebtoken"]` (line 125). The feature now pulls the
  JWT dependency alongside reqwest.
- `Cargo.lock` picked up `jsonwebtoken v9.3.1` (and its transitive crates).

### src/embed/gemini.rs (new)
The adapter module, gated `#[cfg(feature = "embed-gemini")]`.

- `ServiceAccountCredentials` (line ~55): serde shape of the service-account JSON key file
  (`client_email`, `private_key`, `token_uri` optional, `project_id` optional).
- `load_credentials(path)` (line 65): reads/parses the key file; missing/unparseable file is
  `Unavailable` naming the path.
- `trait GeminiTokenSource: Send + Sync + Debug` (line 102): `async fn access_token(&mut self)
  -> Result<String, EmbedError>`. Production impl and an offline fake both implement it, so
  `GeminiEmbedder` logic is fully injectable/offline-testable.
- `ServiceAccountTokenSource` (line ~115): production token source.
  - `mint_jwt()` (line 149): builds claims `{ iss: client_email, scope:
    https://www.googleapis.com/auth/cloud-platform, aud: token_uri, iat: now, exp: now + 3600 }`
    (JWT_LIFETIME_SECS = 3600), signs RS256 with `jsonwebtoken::EncodingKey::from_rsa_pem`.
    A malformed/unusable private key is `Backend` (permanent, operator fixes it).
  - `access_token()`: POSTs urlencoded `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`
    and `assertion=<jwt>` to `token_uri`, parses `access_token`, and caches it until
    `expires_in - 60s` (TOKEN_CACHE_MARGIN). Transport failure -> `Unavailable`; non-2xx or
    missing `access_token` -> `Backend`. `token_uri` defaults to
    `https://oauth2.googleapis.com/token` when absent.
- `GeminiEmbedder` (line ~248): holds `model`, `dim`, a
  `tokio::sync::Mutex<Box<dyn GeminiTokenSource>>`, the full `embed_url`, and a
  `reqwest::Client`. `project`/`location` are constructor inputs only (used to build the URL),
  not stored fields, so there is no dead code under `-D warnings`.
  - `vertex_embed_url(project, location, model)` (line 253): the canonical
    `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:embedContent`.
  - `new(...)` (line ~275): injectable token source + embed URL (tests pass a fake source and
    an httpmock URL, mirroring how bge_m3 injects `base_url`).
  - `model_identity()` (line 288): returns the configured model (`gemini-embedding-001` by
    default), the `EmbeddingContract.model` stamp.
  - `request_embedding(text)`: fetches a Bearer token, POSTs
    `{"content": {"parts": [{"text": text}]}, "outputDimensionality": dim}` to `embed_url` with
    `Authorization: Bearer <token>`. `outputDimensionality` IS sent from the configured `dim`
    (A3-R1-1, corrects the initial omission): gemini-embedding-001 truncates to 768/1536/3072
    via that parameter; A4 owns the construction guard that rejects any other `dim` before an
    unsupported value is sent. Error classification mirrors bge_m3's structure:
    connect/transport failure -> `Unavailable`; any non-2xx -> `Backend`; unparseable body or
    missing values -> `Backend`; width mismatch -> `Backend`.
  - `Embedder::embed`: CON-7 empty/whitespace -> `Unavailable` BEFORE any network; parses
    `embedding.values` (the real `:embedContent` envelope), checks `len == dim`,
    L2-normalizes.
  - `l2_normalize_in_place`: rejects non-finite and zero-norm with `Backend`, gemini-tailored
    error text (mirrors bge_m3).
  - `as_any` returns `Some(self)` so the registry downcast finds the type.

### src/embed/mod.rs
- `mod gemini;` (line 16) and `pub use gemini::GeminiEmbedder;` (line 27), both cfg-gated.
- `EmbedderKind::Gemini::is_ready()` -> `cfg!(feature = "embed-gemini")` (line 200). Flipped
  to true, so `LAMBO_EMBEDDER=gemini` resolves a real adapter when the feature is compiled.
- `gemini_identity(embedder)` (lines 413-425): mirrors `candle_identity`, downcasing via
  `as_any` to `GeminiEmbedder` and returning `model_identity()`. A cfg-not arm returns `None`.
- `build_gemini_embedder(cfg)` (line 441, cfg feature-on): resolves credentials first from
  `gemini_credentials` else `GOOGLE_APPLICATION_CREDENTIALS`; missing -> a clear
  `Unavailable` naming the variable. Then derives `project` (config else `creds.project_id`),
  `location` (default `us-central1`), `model` (default `gemini-embedding-001`), builds a
  `ServiceAccountTokenSource` and the Vertex URL, and constructs the adapter. No network is
  touched at build time (token mint/exchange happen on first `embed`).
- `build_embedder` Gemini arm (line 553): feature-on calls `build_gemini_embedder(&cfg)` and
  feature-off returns `missing_feature(EmbedderKind::Gemini)`.

### src/resolve.rs
- `resolve_backends` model selection (lines 158-165): extended the Candle special-case into a
  match so a `Gemini` embedder stamps `gemini_identity(...)`, yielding
  `EmbeddingContract.model == Some("gemini-embedding-001")` instead of NULL.

## Auth / flow design (why it is shaped this way)

Vertex REST `embedContent` requires a Google OAuth access token. The adapter authenticates as a
service account: it mints an RS256 JWT signed with the service-account private key (PEM), with
`iss` = `client_email`, `aud` = the token endpoint, and a 3600s lifetime, exchanges it at
`token_uri` for an OAuth access token (the `jwt-bearer` grant), and caches the token until a
minute before `expires_in`. This keeps every auth secret inside the adapter, works offline
from build (credentials are only read, never dialed), and lets the embed path be tested with a
fake token source and a mock HTTPS server. Credentials come from an explicit
`gemini_credentials` path when configured, otherwise `GOOGLE_APPLICATION_CREDENTIALS`; an
embedder that cannot authenticate is built only as an error (clear `Unavailable`), never as a
silently-broken embedder.

## A1-A1-1 supersession

The A3 brief required replacing the A1-A1-1 closure test `gemini_feature_on_fail_closed_names_a3`,
which locked the old feature-on arm's "not implemented yet (A3)" fail-closed error. A3 replaced
that arm with a real adapter, so:

- Deleted `gemini_feature_on_fail_closed_names_a3` (it asserted the removed fail-closed
  contract).
- Added `gemini_feature_on_builds_adapter_from_credentials` (mod.rs line 929, cfg feature-on):
  writes a synthetic service-account JSON (with the in-test RSA keypair) to a temp file, builds
  via `build_embedder` with `gemini_credentials` set, asserts `Ok`, and asserts
  `crate::embed::gemini_identity(...)` returns `gemini-embedding-001`. Construction touches no
  network.
- Also updated the two sibling registry tests that asserted the old `is_ready() == false`
  contract, since they would otherwise fail under feature-on:
  - `gemini_is_ready_false_until_a3` -> `gemini_is_ready_requires_feature` (asserts
    `is_ready() == cfg!(feature = "embed-gemini")`).
  - `gemini_fail_closed_no_silent_fallback` -> `gemini_fail_closed_without_credentials`
    (feature-off still names the missing feature; feature-on names the missing credentials via
    `GOOGLE_APPLICATION_CREDENTIALS`/`credentials`).

## Tests (src/embed/gemini.rs tests module, all offline via httpmock + in-test RSA keypair)

- `mints_and_verifies_service_account_jwt`: mints a JWT from a synthetic service-account JSON
  (in-test RSA keypair), decodes with the public key, asserts header alg = RS256 and claims
  iss/aud/exp (exp - iat == 3600).
- `exchanges_jwt_for_access_token_and_caches`: runs the exchange against a mock `token_uri`
  (httpmock), asserts the returned `access_token`, and that a second call hits the cache
  (`assert_hits(1)`).
- `token_endpoint_transport_failure_is_unavailable`: unreachable token endpoint -> `Unavailable`.
- `token_endpoint_http_error_is_backend`: mock 401 -> `Backend`.
- `embeds_and_normalizes`: fake token + mock embed URL returning a valid Vertex payload; asserts
  the vector is unit-norm and dim correct.
- `rejects_empty_and_whitespace_before_network` (CON-7): empty/whitespace -> `Unavailable`
  before any request.
- `transport_failure_is_unavailable`: unreachable embed URL -> `Unavailable`.
- `vertex_http_errors_are_backend`: mock 400/403/500 -> `Backend`.
- `malformed_body_is_backend`: non-JSON -> `Backend`.
- `missing_prediction_is_backend`: `{"predictions": []}` -> `Backend`.
- `rejects_dimension_mismatch`: wrong width (512 vs 768) -> `Backend`, message names 512.
- `rejects_non_finite_and_zero_norm`: NaN/Inf/zero vector -> `Backend`; a valid vector
  normalizes.
- `model_identity_returns_configured_model`: returns `gemini-embedding-001`; `as_any`
  downcast succeeds.

Registry tests in mod.rs: `gemini_is_ready_requires_feature`,
`gemini_fail_closed_without_credentials`, `gemini_feature_on_builds_adapter_from_credentials`
(plus the pre-existing A2 config tests).

## Gates (all run in this worktree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-bge,embed-fixture` | 931 passed / 0 failed |
| `cargo test --features embed-gemini,embed-bge,embed-fixture` | 945 passed / 0 failed |
| `cargo check --no-default-features --features embed-fixture` | pass |
| `cargo check --features ship,embed-gemini` (ship exists) | pass |
| `cargo doc --no-deps --document-private-items --features embed-gemini,embed-bge,embed-fixture` | pass (54 warnings, all pre-existing intra-doc-link lints elsewhere in the crate; none in gemini.rs) |

The `embed::gemini` suite is 13 tests; the three registry gemini tests pass under feature-on;
all 21 gemini-scoped tests pass together.

## Rules honoured
- Work confined to `/home/nryn/work/lambo-wt-a3`; every edit verified via `pwd` +
  `git rev-parse --show-toplevel`. No `git commit`. `.env`, `models/`, and the main checkout
  were not touched. (The main checkout was verified clean after an earlier tool-path slip and
  its two files were restored.)
- No em dashes anywhere in this doc, gemini.rs, the mod.rs/resolve.rs additions, or Cargo.toml.
