# A3 implementation brief: the Gemini (Vertex) embedder adapter

Worktree: `/home/nryn/work/lambo-wt-a3`, branch `a3-gemini-adapter`, off `lambo-for-mooshik`.
A1 (registry) and A2 (config keys) are already merged and present.

## Goal
`src/embed/gemini.rs`: a real Vertex `gemini-embedding-001` adapter. Build the adapter from
config, mint a Google service-account access token (RS256 JWT then OAuth token exchange), call
Vertex `embedContent`, L2-normalize, and honour every inherited Embedder contract. Flip
`is_ready()` to `true` and wire model-identity stamping so the `EmbeddingContract.model` is
`gemini-embedding-001`, not NULL.

## Operator decision (recorded in A-gemini-embedder.md, confirmed by operator)
Vertex + service-account OAuth, adding a crypto dependency. ADC is the fallback: the credentials
are resolved from `gemini_credentials` (explicit path) else `GOOGLE_APPLICATION_CREDENTIALS`; if
neither is present, construction fails (an embedder that cannot authenticate is unusable).

## New dependency (Cargo.toml)
Add an OPTIONAL dep gated behind `embed-gemini`:
```
jsonwebtoken = { version = "9", optional = true }
```
and extend the feature: `embed-gemini = ["dep:reqwest", "dep:jsonwebtoken"]`.
`jsonwebtoken::EncodingKey::from_rsa_pem` signs RS256 and parses the service-account PEM directly;
no other crypto dep is needed.

## The adapter (src/embed/gemini.rs)

### Endpoint and auth flow
- Credentials JSON (service account): fields `client_email`, `private_key` (PEM string),
  `token_uri` (usually `https://oauth2.googleapis.com/token`), `project_id`.
- Access token: build JWT claims `{ iss: client_email, scope:
  "https://www.googleapis.com/auth/cloud-platform", aud: token_uri, iat: now, exp: now + 3600 }`,
  sign RS256 with `EncodingKey::from_rsa_pem(private_key)`, then POST urlencoded
  `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>` to `token_uri`.
  Parse `access_token` from the JSON response. Cache it until ~1 minute before `expires_in`.
- Embed call: `POST
  {location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:embedContent`
  with `Authorization: Bearer <token>` and body
  `{"content": {"content": "<text>"}, "outputDimensionality": <dim>}`. (For tests the adapter
  must allow injecting a base embed URL and a token endpoint, mirroring how bge_m3 injects
  `base_url` from a mock server.)

### Response parsing
`{"predictions": [ { "embeddings": { "statistics": {...}, "values": [floats] } } ] }`.
Take `predictions[0].embeddings.values`. Validate `len == dim` (else `Backend` naming both).
L2-normalize in place, rejecting non-finite and zero-norm with `Backend`.

### Contracts (mirror bge_m3.rs exactly in structure)
- **CON-7**: `embed` rejects empty/whitespace input with `EmbedError::Unavailable` BEFORE any
  network.
- **CON-2**: no retry that can change the request. On failure, return the error; never resend
  with a different body/model.
- **Error classification**:
  - reqwest connect/transport failure -> `Unavailable` (caller degrades to canonical matching).
  - any non-2xx HTTP status from Vertex or the token endpoint -> `Backend` (auth, quota, or a
    wrong model/URL; permanent, operator fixes).
  - unparseable response body or missing prediction/values -> `Backend`.
  - dimension mismatch -> `Backend`.
- Normalization exactly as bge_m3's `l2_normalize_in_place` (reject non-finite, reject
  zero-norm), with gemini-tailored error text.
- Reuse `super::math`/`EmbedError`; do not duplicate the enum.

### Struct shape (testable)
```
GeminiEmbedder {
  project: String, location: String, model: String, dim: usize,
  token_source: Box<dyn GeminiTokenSource>,   // async access_token()
  embed_url: String,    // full embedContent URL, injectable for tests
  client: reqwest::Client,
}
```
`trait GeminiTokenSource { async fn access_token(&mut self) -> Result<String, EmbedError>; }`
with a production `ServiceAccountTokenSource` (mint + exchange + cache) and a
`#[cfg(test)]`-friendly construction path so the embed logic is testable with a fake token and a
mock HTTP server.
`GeminiEmbedder` overrides `as_any()` to expose `model_identity()` returning
`gemini-embedding-001` (the configured model).

## mod.rs wiring
- `mod gemini;` (cfg embed-gemini), `pub use gemini::GeminiEmbedder;`
- `EmbedderKind::Gemini::is_ready()` -> `cfg!(feature = "embed-gemini")` (flip to true).
- `build_embedder` Gemini arm: on feature-off `missing_feature`; on feature-on, resolve
  credentials (explicit path else GOOGLE_APPLICATION_CREDENTIALS; missing -> clear `Unavailable`
  naming the variable), build `GeminiEmbedder` from cfg (project/location/model/dim), and return
  it. Do NOT send an unsupported `outputDimensionality`: dim validation (A4) comes next but the
  adapter must still error if `token_uri`'s creds are unusable.
- Add `gemini_identity(embedder: &dyn Embedder) -> Option<String>` mirroring `candle_identity`
  (downcast via `as_any` to `GeminiEmbedder`, return `model_identity()`), with a cfg-not arm
  returning `None`.

## resolve.rs wiring (embedding-contract model stamp)
In `resolve.rs` near line ~160, the `model` selection currently special-cases only `Candle` and
otherwise reads `llama_model`. Extend it so a Gemini embedder stamps the real model string:
when `embedder_cfg.kind == EmbedderKind::Gemini`, use `crate::embed::gemini_identity(...)`.
Result: `EmbeddingContract.model == Some("gemini-embedding-001")` instead of NULL.

## Supersede the A1-A1-1 test
`src/embed/mod.rs` test `gemini_feature_on_fail_closed_names_a3` (cfg embed-gemini) asserts the
old fail-closed arm returns "not implemented yet (A3)". A3 replaces that arm with a real adapter,
so that test MUST be replaced: feature-on now builds a real `GeminiEmbedder` given valid config,
and errors (naming the failure) when config/creds are missing or invalid. Update or replace it;
do not leave a test asserting the old contract. Record what you did.

## Tests to add (gemini.rs tests module, offline, no real network / no real key)
- Token minting: generate an RSA keypair in-test, build a synthetic service-account JSON,
  run the exchange against a mock `token_uri` (httpmock), assert `access_token` is returned and
  the JWT verifies (decode with the public key; check header alg=RS256 and claims iss/aud/exp).
- Embed: build a `GeminiEmbedder` with a fake token source + a mock embed URL returning a valid
  Vertex payload; assert the vector is L2-normalized (unit norm) and dim correct.
- CON-7: empty and whitespace input -> `Unavailable` before any request.
- Dimension mismatch: mock returns wrong width -> `Backend`.
- Error classification: transport failure -> `Unavailable`; mock returns 4xx/5xx -> `Backend`;
  malformed body -> `Backend`.
- Non-finite and zero-norm vectors -> `Backend`.
- `model_identity()` returns `gemini-embedding-001`.
Use the existing bge_m3 test style (httpmock, naming conventions).

## Deliverable
`dev-diary/lambo-for-mooshik/a-run/A3-implementation.md` in THIS worktree: what changed
(file:line across Cargo.toml, src/embed/mod.rs, src/embed/gemini.rs, src/resolve.rs), the
auth/flow design, every test, every gate result, and how the A1-A1-1 test was superseded.

## Gates (run ALL, report exact counts)
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --all-targets --features embed-gemini -- -D warnings`
- `cargo test --features embed-bge,embed-fixture` (baseline regression)
- `cargo test --features embed-gemini,embed-bge,embed-fixture` (new gemini tests)
- `cargo check --no-default-features --features embed-fixture`
- `cargo check --features ship,embed-gemini` if ship exists, else `cargo check --features embed-gemini`
- `cargo doc --no-deps --document-private-items --features embed-gemini,embed-bge,embed-fixture`

## Hard safety rule
Work ONLY in `/home/nryn/work/lambo-wt-a3`. Before every edit run `pwd` and
`git rev-parse --show-toplevel` and verify the top-level is `/home/nryn/work/lambo-wt-a3`.
NEVER edit `/home/nryn/work/lambo` (the main checkout). Two prior agents contaminated the main
checkout; do not repeat.

## Rules
- NEVER commit. Leave the tree dirty.
- No em dashes in any prose, comment, or doc.
- Never touch `.env`, `models/`, or anything outside the worktree.
