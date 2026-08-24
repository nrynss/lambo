# Adversarial review: mooshik A (Gemini/Vertex embedder adapter), round 1

**Reviewer**: independent adversarial reviewer, agent_id `A3Review`. Wrote nothing under
review except this file.
**Scope**: the uncommitted A3 implementation in worktree `/home/nryn/work/lambo-wt-a3`
(branch `a3-gemini-adapter`): new `src/embed/gemini.rs`, plus edits to `Cargo.toml`,
`src/embed/mod.rs`, `src/resolve.rs`, reviewed STRICT read-only against the A3 brief
(`a-run/A3-implementation-brief.md`), the spec sections A3/A4 of
`lambo-for-mooshik/A-gemini-embedder.md`, and the implementation record
(`a-run/A3-implementation.md`).
**Verdict**: **REQUEST_CHANGES** (1 P1 / 1 P3). The P1 (outputDimensionality omitted) is an
explicit deviation from the A3 spec that makes every valid non-native dimension
(including the spec's recommended 1536 and the crate's own 1024 default) fail at embed.
Everything else adversarially verified clean.

## The outputDimensionality deviation: adjudicated DEFECT (A3-R1-1, P1)

**Spec requirement (evidence):**
- A3, `A-gemini-embedder.md:61`: "Vertex `gemini-embedding-001`, `outputDimensionality`
  from `cfg.dim`." This is an explicit A3-scope requirement.
- A4, `A-gemini-embedder.md:84`: "`gemini-embedding-001` truncates to **768, 1536 or 3072**
  only." The verb "truncates" confirms the model's output width is a runtime parameter
  selected via `outputDimensionality`, not a fixed width.
- `A-gemini-embedder.md:108`: "**Recommended default: 1536**" (the intended deployment
  dimension is not the model's native width.
- Google Vertex reference (verified via web search): for `gemini-embedding-001`
  `outputDimensionality` defaults to the model's full output size, 3072. So omitting it
  makes Vertex return a 3072-wide vector.

**What the implementation does:** `request_embedding` (`gemini.rs:301-304`) sends only
`{"content": {"content": text}}` and deliberately omits `outputDimensionality`, justified by
a doc-comment (`gemini.rs:12-16, 302-304`) asserting "gemini-embedding-001 has a fixed
output width that cannot be adjusted". That premise is factually wrong: the width is
adjustable to 768/1536/3072 via `outputDimensionality` (per A4 and the Vertex reference).

**Consequence (the harm):** without `outputDimensionality`, Vertex returns 3072 dims. The
adapter then enforces `len == dim` (`gemini.rs:378-384`) and returns `Backend` on mismatch.
So:
- `cfg.dim = 768` (valid per A4): 3072 vs 768 -> `Backend` on every embed. FAILS.
- `cfg.dim = 1536` (valid per A4 AND the spec's recommended default): FAILS.
- `cfg.dim = 1024` (the crate's own `default_embed_dim`, and what the A2 default config
  produces): FAILS. So the out-of-the-box `LAMBO_EMBEDDER=gemini` path is broken.
- Only `cfg.dim = 3072` (the model's native width) passes the check.

So yes: a user configuring a valid non-default dim (768 or 1536) gets a wrong-width 3072
vector and fails the dim-mismatch check; only the native width is usable. The omission is a
real, functional defect in the A3 deliverable, not a cosmetic detail. It fails loudly
(`Backend`, never silent vector poisoning) and is trivial to remediate, but it blocks the
primary and recommended configurations, so it is P1.

**Recommended correct behaviour:** send `outputDimensionality = dim` in the embed request
body when `dim` is within the supported set {768, 1536, 3072}. Pair it with A4's
construction guard (the next phase) that rejects any other configured dim at construction
naming the three, so an unsupported value (e.g. the 1024 default) is refused before any
network rather than surfacing as a Vertex `Backend` at first embed. Reposition the
misleading "fixed width" doc-comment to state the supported set and that the A4 guard
enforces it.

## Findings

- **A3-R1-1 (P1)** `outputDimensionality` is omitted from the embed body, violating A3's
  explicit `outputDimensionality from cfg.dim` requirement and A4's "truncates to
  768/1536/3072" model. Every valid non-native dim (768, the recommended 1536) and the
  crate's 1024 default fail the width check as `Backend`; only 3072 works. See adjudication
  above.
- **A3-R1-3 (P3)** `embeds_and_normalizes` (`gemini.rs:547-567`) mocks a response already
  at the expected width and asserts only method/path/auth-header, never the request body.
  It is mutation-blind to the A3-R1-1 omission (and to the `outputDimensionality` value
  generally), which is precisely why the omission landed uncaught. Add a body assertion that
  pins `outputDimensionality == dim` (as part of the A3-R1-1 fix) so the contract is locked
  by a test.

## Adversarial verification (everything else: clean)

Method note: strictly read-only, so mutation checks were done by analysis of each test
(would the named mutation be caught) rather than transient source edits.

- **JWT mint**: `mint_jwt` (`gemini.rs:149-170`) builds `Claims{iss, scope, aud, iat, exp}`
  with `scope = https://www.googleapis.com/auth/cloud-platform`, `aud = token_uri`,
  `exp = iat + 3600`, signs RS256 via `jsonwebtoken::EncodingKey::from_rsa_pem`, malformed
  key -> `Backend`. Test `mints_and_verifies_service_account_jwt` decodes with the matching
  public key, asserts `alg == RS256`, `iss`, `aud`, `exp-iat == 3600`; a wrong signature
  (unsigned/other key) would fail decode. Solid.
- **OAuth exchange + parse + TTL cache**: `access_token` (`gemini.rs:175-221`) POSTs
  urlencoded `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer` + `assertion` to
  `token_uri`, parses `access_token`, caches until `expires_in - 60s` (saturating, min 1s,
  default 3600). `exchanges_jwt_for_access_token_and_caches` asserts both the token and
  `assert_hits(1)` on the second call (would fail if the cache were broken). Correct.
- **Vertex embedContent**: `vertex_embed_url` (`gemini.rs:253-257`) builds the canonical
  endpoint; request carries `Authorization: Bearer <token>`. URL and auth verified by
  `embeds_and_normalizes` (`method/path/header`). Correct.
- **Response parse**: `predictions[0].embeddings.values` via serde (`gemini.rs:224-238`,
  `370-377`); empty `predictions` -> `Backend`. Correct.
- **width == dim**: `gemini.rs:378-384` -> `Backend` naming both widths;
  `rejects_dimension_mismatch` asserts `Backend` and the observed width "512". Solid.
- **L2 normalize**: `l2_normalize_in_place` (`gemini.rs:335-355`) rejects non-finite and
  zero-norm -> `Backend`; `rejects_non_finite_and_zero_norm` covers NaN/Inf/zero and a valid
  vector; `embeds_and_normalizes` asserts unit norm on a non-unit input. Solid.
- **CON-7**: trim check is the first statement of `embed` (`gemini.rs:363-369`), before any
  token mint or request; `rejects_empty_and_whitespace_before_network` asserts `Unavailable`
  for `""`, `"   "`, `"\t\n "`. Correct.
- **CON-2**: no retry loop anywhere in the token or embed path; failures propagate as-is.
  Correct.
- **Error classification**: transport (token endpoint and Vertex) -> `Unavailable`;
  any non-2xx -> `Backend`; unparseable/missing-prediction/width-mismatch/non-finite ->
  `Backend`. Three tests (transport, vertex 400/403/500, token 401) plus the malformed-body
  and missing-prediction tests each assert the classification would fail if mis-shuffled.
  Correct.
- **as_any / model_identity**: `as_any -> Some(self)` (`gemini.rs:389-391`);
  `model_identity` returns the configured model; test asserts the downcast and the string.
  Correct.
- **resolve.rs stamp**: match on kind routes Gemini to `gemini_identity(..)`
  (`resolve.rs:158-165`), stamping `Some("gemini-embedding-001")`; `_ => llama_model` keeps
  bge/fixture behaviour. Exhaustive, cfg-not arm returns `None`. Correct.
- **is_ready**: `EmbedderKind::Gemini.is_ready() -> cfg!(feature = "embed-gemini")`
  (`mod.rs:200`), true under the feature; `gemini_is_ready_requires_feature` asserts the
  exact `cfg!` equality under both configurations. Correct.
- **build_gemini_embedder creds**: resolves `gemini_credentials` else
  `GOOGLE_APPLICATION_CREDENTIALS`; missing -> clear `Unavailable` naming the variable
  (`mod.rs:445-457`); then project (config else `creds.project_id`), location default
  `us-central1`, model default `gemini-embedding-001`; no network at build. Correct.
- **Cargo / feature gating**: `jsonwebtoken = {version = "9", optional = true}` and
  `embed-gemini = ["dep:reqwest", "dep:jsonwebtoken"]`; additivity holds (no other feature
  or default pulls jsonwebtoken). `-D warnings` clippy passes with and without the feature.
  Correct.
- **A1-A1-1 supersession**: the old fail-closed test `gemini_feature_on_fail_closed_names_a3`
  (asserting "not implemented yet (A3)") is gone; grep confirms no stale reference. Replaced
  by `gemini_feature_on_builds_adapter_from_credentials` (feature-on: writes a synthetic
  service-account JSON with the in-test RSA key, builds via `build_embedder`, asserts
  `Ok` and identity `gemini-embedding-001`, no network). Sibling `is_ready` and fail-closed
  tests were updated to the new (cfg-aware / feature-on-credentials) contracts rather than
  left asserting the removed arm. Sound.
- **Registry feature on/off tests**: `gemini_is_ready_requires_feature` uses the `cfg!`
  macro inside an ungated test (correct under both); `gemini_fail_closed_without_credentials`
  uses `#[cfg(feature)]`/`#[cfg(not(feature))]` inner asserts (correct under both).
- **No em dashes** introduced in any added line (gemini.rs and the implementation record
  score 0; the two hits in mod.rs/resolve.rs are pre-existing context, and the single hit in
  the Cargo.toml diff is unchanged context).

## Gates (my own runs, worktree `/home/nryn/work/lambo-wt-a3`, pristine)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-bge,embed-fixture` | 931 passed / 0 failed / 1 ignored |
| `cargo test --features embed-gemini,embed-bge,embed-fixture` | 945 passed / 0 failed / 1 ignored |
| `cargo check --no-default-features --features embed-fixture` | pass |
| `cargo check --features embed-gemini` | pass |

All gates pass and the test counts exactly match the implementation record (931 / 945).
None of the passing gates exercise a live Vertex call, and none asserts the embed request
body, so the gate suite does not and cannot expose A3-R1-1.

## Blocker

Fix A3-R1-1 (send `outputDimensionality = dim` for dim in {768, 1536, 3072}, plus the A4
construction guard for values outside the set) and add the body-pinning test (A3-R1-3)
before this A3 deliverable is considered done.

- A3Review, 2026-08-24
