# A3 live-Vertex verification and defect fix (2026-08-24)

Workstream A's live leg was previously operator-unverified (no key in CI, live test
`#[ignore]`d). The user provided the machine's Google ADC (`authorized_user`,
`~/.config/gcloud/application_default_credentials.json`, project `nryn-personal`), which drives
a REAL end-to-end run against Vertex. That run exposed and fixed a genuine defect and added a
missing grant type.

## Defect found by the live probe (P1-class)

The adapter's `:embedContent` request and response were both wrong against the real API:

| Aspect | Draft (wrong) | Real Vertex | Fixed |
| --- | --- | --- | --- |
| request `content` | `{"content": {"content": text}}` | `{"content": {"parts": [{"text": text}]}}` | `parts.text` |
| response envelope | `predictions[].embeddings.values` | `{"embedding": {"values": [...]}}` (top-level `embedding`) | `embedding.values` |

Vertex rejected the draft body with `Unknown name "content" at 'content'`, and the draft
response parser would have found no `predictions`. The offline mocks had fabricated the wrong
envelope, so only the live leg could catch it. This is exactly the gap the E2E P2 (missing live
test) warned about; running it paid off.

## Added: authorized_user ADC support

The available credential is a Google `authorized_user` ADC (client_id + refresh_token), not a
service-account key. The adapter previously only did the service-account `jwt-bearer` grant.
Changes:

- `GoogleCredentials` enum: `ServiceAccount` (client_email + private_key + project_id) and
  `AuthorizedUser` (client_id + client_secret + refresh_token + quota_project_id), each with a
  `token_uri`.
- `load_credentials` dispatches on the JSON `type` field (defaults `service_account`),
  validating required fields and defaulting `token_uri` to Google's.
- `GoogleOAuthTokenSource` (replaces `ServiceAccountTokenSource`): `jwt-bearer` grant for
  service-account, `refresh_token` grant for authorized_user; cache until `expires_in - 60s`.
- Project resolution: `gemini_project` config, else `GoogleCredentials::project_id()`
  (service-account project / ADC quota project), else error.

## Live verification

Gate: the former `#[ignore]`d live test `gemini_live_embeds_against_vertex` now PASSES against
real Vertex using only `GOOGLE_APPLICATION_CREDENTIALS` (authorized_user ADC), resolving the
project from `quota_project_id`, no `LAMBO_GEMINI_PROJECT`:

```
GOOGLE_APPLICATION_CREDENTIALS=~/.config/gcloud/application_default_credentials.json \
  cargo test --features embed-gemini --lib \
  embed::gemini::tests::gemini_live_embeds_against_vertex -- --ignored
```

Observed: OAuth refresh grant -> HTTP 200; `outputDimensionality=1536` honored (real width
1536); L2-normalized output. Live probe also confirmed the non-set dims: 768/1536/3072 all
return their requested width; empty text is refused by Vertex (400), which CON-7 intercepts
first.

## Offline test updates

- Mock embed response envelope updated to `{"embedding": {"values": [...]}}`.
- `embeds_and_normalizes` now pins the request body (`outputDimensionality`, `parts`, `text`).
- `missing_embedding_is_backend` (was `missing_prediction_...`) mocks the real shape.
- New `authorized_user_uses_refresh_token_grant_and_caches`.
- `pub(crate) mod gemini` so the resolve-level test can reach the test key.

## Gates (re-run on the fixed tree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib gemini` | 24 passed / 0 failed / 1 ignored (live) |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 passed / 0 failed (no regression) |
| `cargo check --no-default-features --features embed-gemini,store-memory` | pass |
| `cargo check --features ship,embed-gemini` | pass |
| LIVE `gemini_live_embeds_against_vertex` (`--ignored`) | passed against real Vertex |

Live CI row and real GitHub-runner remain operator-verified; the offline unit surface plus the
locally-run live test cover the change here.
