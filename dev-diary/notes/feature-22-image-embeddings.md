# #22 PR 5: EmbeddingGemma 2 over llama.cpp (decisions)

Base: main `63df918` plus the orchestrator's `embed-eg2 = ["embed-bge"]` feature
commit. That feature line differs from the amendment's approved R13 text,
`embed-eg2 = ["dep:reqwest"]`: building on `embed-bge` reuses the #21 client module
(`bge_m3.rs`), which `eg2` wraps, at the cost of also compiling the `bge_m3` kind into
an eg2-only build. No new crate either way, and `ship` carries both. Design of record: `lambo-handoff-2026-10-08/design/22-DESIGN.md` (sections 2,
3, 7.3, 8) as amended by `22-AMENDMENT-PR5.md` (owner approved R20 on 2026-10-09; R11's
`lambo-eg2-v1` profile and the artifact-naming contract are implemented as the
defaults). The fifth of the #22 PRs: the server-side embedder that makes
`lambo_derive_image` with an image (not a vector) work against a real model. Live
evidence: `evidence/issue-22-eg2/`.

## What changed

| piece | where |
|---|---|
| `post_json` (generic body and response) and `StatusRule` / `StatusVerdict` / `default_status_rule`, split out of `request_embedding` with no behaviour change | `src/embed/bge_m3.rs` |
| `EmbeddingGemma2Embedder`, the profile constants, `Eg2ServerCheck`, `image_status_rule`, the `/props` judge, MRL | `src/embed/eg2.rs`, httpmock tests in `src/embed/eg2/tests.rs` |
| `EmbedderKind::EmbeddingGemma2` (`embeddinggemma2`, aliases `embeddinggemma-2`, `eg2`), `[embedder] images`, `eg2_identity`, the registry arm, `api_key_env` accepted for this kind | `src/embed/mod.rs` |
| the contract `model` stamped from `eg2_identity` | `src/resolve.rs` |
| the live test (AC2) | `tests/live_eg2.rs` |

## Decisions

**A layer on the #21 client, not a fork of it.** `EmbeddingGemma2Embedder` holds a
`BgeM3LlamaCppEmbedder` and sends its own bodies through the new `post_json`. So the
bearer header, the https-or-loopback rule, no redirects, the proxy bypass for loopback
http, the capped and scrubbed error body and the J3 status table are the same code,
not a copy. The only seam is the status rule (below).

**The profile `lambo-eg2-v1` pins four things**: the two model-card text prefixes
(`title: none | text: ` for `embed`, `task: search result | query: ` for
`embed_query`), no image prefix, the fixed 280-token image budget, and
truncate-then-normalize. Any change is a new profile name, so a new contract.

**The contract `model` is `<artifact>;prompts=lambo-eg2-v1`.** `llama-server` ignores
the request's model name (a wrong name returns the same vector with 200), so the
contract names the weights: by default `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0`.
The artifact is also sent as the request's `model` (ignored by llama-server; an Ollama
deployment would need its tag there). A `;` in the artifact is refused, so the profile
suffix is unambiguous. This replaces design 3.2's `google/embeddinggemma-2[@rev]`
default, per amendment R11.

**`dim` must be 768, 512, 256 or 128, and the global 1024 default is refused** with
"set [embedder] dim = 768". `EmbedderConfig.dim` stays a plain `usize`; making it
optional to default per kind would touch every config literal for one message.

**MRL: check, truncate, normalize.** The server must return the native 768 (a 1024
answer means BGE-M3 is behind the kind, a 256 answer a server that truncated without
the profile); every component must be finite, the discarded tail included; then
truncate, then L2-normalize, refusing a zero norm over the kept head.

**Image `500`s are refined by body, for image calls only.** b11517 answers an image
sent to a server without `--mmproj` with `500 ... provide the mmproj`, and an
undecodable image with `500 Failed to load image or audio file`. The J3 table reads a
500 as a busy server (transient), so both would retry forever. `image_status_rule`
classes the first `PermanentConfig` with a hint naming `--mmproj` and the second
`Content`; any other image 500 stays transient, and text calls use the plain table.
The class is still decided where the response is read (J1-R2-2); the rule only lets
the adapter that knows its server's bodies refine it.

**The 280-token budget is checked on every image response** (superseded by the
review remediation below: an image's count does not show the budget). Found live: at
llama-server's default ubatch of 512, `--image-max-tokens 280` is silently capped to
256 (260 tokens with framing). That server is still size invariant, so the size
check the amendment planned would pass on it, but its vectors are off the profile
(cosine 0.9989 to the 280 server). `/props` does not report the budget. The adapter
reads `usage.prompt_tokens` and refuses an image outside 280 to 312 tokens (b11517
reports 293) as a permanent configuration error naming the flags; a response without
`usage` is not judged. The documented command line adds `--ctx-size 8192
--batch-size 8192 --ubatch-size 8192`, which also lets a long text fit the single
ubatch a non-causal model needs.

**The `/props` startup check is lazy and best effort.** Resolve and `build_embedder`
are synchronous, so the check runs before the first embed. b11517's `/props` reports
`model_path`, `model_ftype` and `modalities.vision` (verified; it does not report the
image budget or pooling). Rules:

- the file name, case and separators folded, must contain `embeddinggemma2`. This
  catches EmbeddingGemma 1 (also 768-d) and BGE-M3 behind the kind. Only the file
  name is shown, never the directory;
- `model_ftype` must equal the quantization the artifact names (the segment after the
  last `/` following `@revision`), when both are present;
- vision `false` refuses images while `images` is on; text still embeds.

`Verified` and `NotExposed` (no `/props`, no `model_path`, not JSON: a hosted
endpoint or Ollama) are kept, `NotExposed` with one warning. A mismatch is never kept,
and an image call asks again while the kept answer says "no vision", so fixing the
server needs no Lambo restart; each failure message is logged once. A 5xx or
unreachable `/props` is skipped and asked again. `without_server_check()` exists for a
server whose `/props` misleads, and for the live test that needs the server's own
mmproj 500.

**`images` is a TOML key only.** `[embedder] images` (default on) is refused for every
other kind, like `api_key_env`. No `LAMBO_*` overlay, so `RESOLVE_ENV_VARS` and its
coverage table are unchanged. It is not part of the contract: turning images off
changes what is embedded, not the space.

## Ranking parity (design 7.3) and the PR 3 cold-start question

Measured in the live test; table and discussion in `evidence/issue-22-eg2/README.md`.
In short: a modality gap of about 0.08 (relevant images 0.75 to 0.79, relevant text
0.82 to 0.87, irrelevant about 0.61 for both); every EG2 cosine, irrelevant ones
included, sits above `RECENT_SCORE = 0.35`; the 0.85 merge threshold is safe for EG2
text but under-merges. The cold-start loss PR 3 recorded (a fresh perfect match at
0.5 under older daemon-scored noise at 0.533) holds for both modalities (fresh image
about 0.38, fresh text about 0.42) and is spec 8 behaviour for any fresh concept. No
constant changed here (design 7.3: measured, not adjusted); it is evidence for Q15.

## Tests

| test | runs in |
|---|---|
| `embed::eg2::tests::*` (25 after the review remediation; 18 before): byte-exact text bodies with the role prefixes, the image body, empty text, MRL at every width, width / non-finite / zero-norm refusals, status classes with the image 500 refined, the token budget, `images = false`, the `/props` check (verified once, mismatch until fixed, no vision, not exposed, 5xx retried, bearer on `/props`), the judge, the kind and contract string, build refusals, resolve stamping the contract | rows with `embed-eg2` (`resolve_stamps_the_eg2_contract` also needs `store-memory`) |
| `embed::tests::{toml_kind_aliases_match_from_str, kind_feature_names}` gain the EG2 kind | every row |
| `tests/live_eg2.rs` (2, ignored): AC2 and the no-projector case | live only, `LAMBO_EG2_URL` / `LAMBO_EG2_TEXT_ONLY_URL` |

Mutation-checked: routing image calls through the default rule, swapping the two
prefixes, and normalizing before truncating each turn the named tests red.

## Review remediation (2026-10-09)

The Opus review (M1, M2, L1 to L5) and the measurements behind each fix:

- **An image's token count does not show the budget (L3, and a defect the review did
  not see).** The 280 to 312 window rested on "b11517 reports 293 for every image",
  which holds only for small, squarish images. Measured at the profile's budget: 236
  to 540 prompt tokens across 100 sizes and shapes, 260 for any square of 800 px or
  more, 250 for 2000x330 (`evidence/issue-22-eg2/image-token-counts.txt`). So ordinary
  photos were refused on a correct server, and narrowing the window to 293 (the
  review's ask) would have refused more. The counts of a capped or default budget
  overlap the range, so no window works. The budget is now checked with a reference
  image (a 1x1 PNG: 293 at the profile, 260 capped, 85 dynamic, 328 at 300), embedded
  before the first image for a server `/props` verified and again every 60 s, within
  8 tokens; an image's own count is not judged. The same data shows the fixed budget
  does not make every render of a picture one vector (a solid square is 0.9989 apart
  between 768 and 896 px, the capped server's own offset; a checkerboard 0.97 to 0.99).
  Whether the profile should downscale images to 768 px before sending is an open
  question for the owner; it would be a new profile name.
- **M1: re-check.** A kept `/props` answer is trusted for 60 s
  (`EG2_PROPS_RECHECK_INTERVAL`) and dropped when an embed fails as unavailable. Once
  verified, a re-check that cannot run holds embeds back (transient) and a server that
  stops reporting its model is refused.
- **L4: backoff.** A check that cannot run is logged once per run of failures and
  retried after 1 s doubling to 30 s, not on every embed.
- **L1: quantization names.** Both sides reduce to the file token through llama.cpp's
  `llama_ftype_name` table (b11517 source and its libllama strings; `(guessed) ` is a
  prefix there, not a suffix). b11517 reports `Q8_0` for the default GGUF (live).
- **L2: no count from a verified server** refuses images, naming the build.
- **L5 (pre-existing, both adapters): "increase the physical batch size"** is now a
  content refusal. Verified live that the documented command line fits 7,008 tokens
  and refuses 9,808 (`evidence/issue-22-eg2/long-text.txt`).

## Not in this PR

- The CI row for `embed-eg2` (`cargo clippy --all-targets --features embed-eg2 -- -D
  warnings` and `cargo test --features embed-eg2`): the orchestrator's, since agents
  do not edit workflows. `embed-eg2` joined `ship` in this PR (5e2a640), after its live
  test passed.
- Recall by image or by vector: PR 6.
- The fidelity reference against transformers / sentence-transformers (amendment
  section 3), blocking for Dresscode's browser vectors, owner-run.
- An Ollama deployment of this kind (untested; it would run without the `/props`
  check).
