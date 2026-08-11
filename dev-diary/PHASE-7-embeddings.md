# P7 — Embeddings & hybrid matching

```yaml
id:       P7
requires: [P1, T0.3]   # T0.4 only for T7.1 Bedrock; default path is BGE-M3 (T7.0)
blocks:   nothing hard — capability-gated; hybrid falls back to Canonical (not keyword-as-product)
parallel: high   # T7.0 done; T7.1 ‖ T7.2 ‖ T7.3runs-parallel-with: P2, P3, P4, P5, P6
```

**Goal:** Portable `Embedder` trait (1024-d dense) + hybrid concept matching (spec §7.1
step 6) + live `vector_candidates` — Cockroach **Distributed Vector Indexing** (spec §12.1)
doing real work: merging concepts normalization can't ("register user" / "create account").

**Default embedder:** **BGE-M3** weights from **Hugging Face**, runtime **llama.cpp**
(see [`notes/embeddings-portable.md`](notes/embeddings-portable.md)).  
**Swap-in:** Bedrock Titan V2 when account is authorized.  
**Tests:** `FixtureEmbedder` only.

**Level B packaging** (see [`notes/level-b-pluggability.md`](notes/level-b-pluggability.md)):
features `embed-bge` (default), `embed-fixture` (default), `embed-bedrock` (optional);
registry `embed::build_embedder`; process start prefers `resolve_backends`. **Dim is not
hardwired** — embedder factory accepts any `dim > 0`; store×embedder match is
`GraphStore::vector_dimensions()` at resolve. Session model space is
`EmbeddingContract { kind, model, dim }` (stamp + check on hybrid write / serve attach).

**Degradation contract:** if hybrid cannot embed, fall back to `MatchStrategy::Canonical`
and log once — not "keyword as product." §12.1 still requires the vector index in use for
the demo (T7.3 + one hybrid merge minimum).

---

### T7.0 — Embedder factory + BGE-M3 / llama.cpp ★ (default path)
```yaml
requires:   T1.3, T1.5
fixture-ok: yes
owns:       src/embed/mod.rs, src/embed/bge_m3.rs, scripts/fetch-bge-m3.sh, scripts/run-llama-embed.sh
status:     done
feature:    embed-bge
```
- Level B: feature `embed-bge` (default-on); `build_embedder(EmbedderKind::BgeM3)`.
- `LAMBO_EMBEDDER=bge_m3|bedrock|fixture` and/or `lambo.toml` `[embedder]`,
  `LAMBO_EMBED_DIM=1024`, `LAMBO_LLAMA_EMBED_URL`.
- Download GGUF (or convert) from HF into `models/` (gitignored); serve with llama.cpp
  `--embedding`.
- HTTP client returns L2-normalized 1024-d vectors.
- **Never mix** BGE-M3 and Titan vectors in one session without re-embed.

**Done when:** smoke against local llama.cpp returns 1024 dims; fixture path still green in CI
without models; binary without `embed-bge` fails closed if `kind=bge_m3`.

---

### T7.1 — `BedrockEmbedder` (optional swap-in)
```yaml
requires:   T1.3, T1.5, T0.4
fixture-ok: yes   # written from the T0.4 handoff; live call behind an integration gate
owns:       src/embed/bedrock.rs
status:     not-started
feature:    embed-bedrock
```
Titan Text Embeddings V2, 1024-dim, gated on `embed-bedrock`, registered in `build_embedder`
for `EmbedderKind::Bedrock`. Via `aws-sdk-bedrockruntime` or Bearer API key, using
T0.4 shapes. Selected when `LAMBO_EMBEDDER=bedrock` / `lambo.toml` and account is
AUTHORIZED. Timeout + typed errors; **embed failure fails the hybrid match step, never the
write** — fall back to canonical match (per-call fallback is the v0.1 shape).

**Done when:** unit tests with a mocked client pass under `--features embed-bedrock`;
`build_embedder(kind=bedrock)` returns a working adapter (not a stub); live smoke (ignored
gate) returns 1024 dims when AWS allows; without the feature, selection fails closed.

---

### T7.2 — Hybrid matching (canonicalization step 6)
```yaml
requires:   T2.2, T1.3
fixture-ok: yes   # FixtureEmbedder near/far pairs (T1.3) drive all tests
owns:       src/graph/hybrid.rs
status:     not-started
```
On canonical-key miss under `MatchStrategy::Hybrid`: embed, query
`store.vector_candidates()`, accept above `semantic_match_threshold=0.85`, create a
`Semantic` edge to the matched concept (decaying, per spec §5). Below threshold or
capability absent → create new concept, keyword-only, log the fallback once per session.
Sits behind T2.2's `Unmatched` seam — do not modify `canonical.rs`.

**EmbeddingContract:** on first embed in a session, stamp `GraphSnapshot.embedding`. On
later hybrid writes, `ensure_compatible` with the live contract — refuse kind/model/dim
swaps (BGE vs Titan is the common trap; same dim is not enough).

**Done when:** with `FixtureEmbedder`, the near pair merges with a `Semantic` edge and the
far text creates a fresh concept; with a no-capability store, behavior is byte-identical to
`MatchStrategy::Canonical`; swapping embedder kind mid-session errors without re-embed.

---

### T7.3 — Live `vector_candidates` on CockroachDB ★ (hackathon requirement)
```yaml
requires:   T3.2, T0.3
fixture-ok: no
owns:       (vector paths inside src/store/cockroach.rs — same owner as T3.2; claim jointly or sequence)
status:     not-started
feature:    store-cockroach
```
The T0.3 spike productionized: embedding column write in `flush()`, index-backed
similarity query, `Capabilities::VECTOR_SEARCH` advertised. Verify with `EXPLAIN` that the
vector index is actually used — "we used the vector index" must be true on camera.
Integration tests under `--features store-cockroach`.

**Done when:** integration test: two paraphrase concepts derived through the full live
stack merge via the index, and `EXPLAIN` output is captured into `dev-diary/evidence/`.

---

## Exit criteria

- [x] BGE-M3 + llama.cpp path documented and smokeable (default, `embed-bge`) — T7.0
- [ ] Bedrock path optional swap-in under `embed-bedrock` (same 1024-d contract) — T7.1
- [ ] Hybrid merge demonstrated offline (fixtures) and live (Cockroach) — T7.2 / T7.3
- [ ] Degraded mode proven equivalent to Canonical strategy
- [ ] `EXPLAIN` evidence of index use committed
- [x] Level B: embedder registry + features fail closed for missing kinds

## Handoff Log

- **2026-08-10:** Portable embeddings decision — default BGE-M3 (HF + llama.cpp), Bedrock
  Titan when authorized. Dim 1024. Details: `notes/embeddings-portable.md`.
- **2026-08-11:** Level B packaging (T1.5) — features `embed-bge` / `embed-fixture` /
  `embed-bedrock`; `build_embedder` fail-closed; see `notes/level-b-pluggability.md`.

---

### T7.0 — BGE-M3 / llama.cpp embedder + factory (2026-08-11) — DONE

- **What exists now:**
  - `src/embed/bge_m3.rs` — `BgeM3LlamaCppEmbedder`, talks to llama.cpp's OpenAI-compatible
    `POST /v1/embeddings` (chosen as the most version-stable surface), parses `data[0].embedding`,
    enforces exact dim (default 1024), **L2-normalizes in place**, and rejects empty text,
    non-finite (NaN/Inf) vectors, zero-norm, dim mismatch, empty data, non-2xx, and bad JSON.
    Includes `.check_health()` against `{base}/health`.
  - `src/embed/mod.rs` — `EmbedderKind` (bge_m3 | bedrock | fixture, parse from `LAMBO_EMBEDDER`
    default bge_m3) + `EmbedderConfig::from_env()` + `build_embedder()` / `embedder_from_env()`
    factory. `bedrock` returns `EmbedError::Unavailable` with a clear "use bge_m3/fixture" note
    (T7.1 will implement it). `fixture` requires dim 1024 exactly.
  - `scripts/fetch-bge-m3.sh` — HF GGUF download into `models/` (gitignored). Default repo
    `gpustack/bge-m3-GGUF` file `bge-m3-FP16.gguf`, overridable via `LAMBO_BGE_M3_HF_REPO` /
    `LAMBO_BGE_M3_GGUF`. Uses `hf` (hub CLI) first, then legacy `huggingface-cli`, then curl
    fallback; `--dry-run` prints plan.
  - `scripts/run-llama-embed.sh` — starts `llama-server -m <model> --embedding -c <ctx>` on the
    parsed port; `--check` health-checks an existing server; refuses if a server is already up.
  - `.env.example` — added `LAMBO_LLAMA_MODEL` (model id sent; empty => server default) and
    `LAMBO_BGE_M3_GGUF`. `LAMBO_LLAMA_MODEL` or `LAMBO_BGE_M3_MODEL` both feed the request model id.
- **Weights downloaded + live smoke PASSED (2026-08-11):**
  - Repro: `scripts/fetch-bge-m3.sh` default = repo **`gpustack/bge-m3-GGUF`** @
    `2d48f1737679ad900d5c26c5aad5410e9c70fdca` (last modified 2024-10-31), file
    **`bge-m3-FP16.gguf`** (1.08 GB, gitignored). Originally tried `bge-m3-f16.gguf` — **does not
    exist**; the repo's full-precision file is `bge-m3-FP16.gguf`. Also available: Q8_0 (0.59 GB),
    Q5_K_M (0.44 GB) if a smaller build is preferred.
  - `./scripts/run-llama-embed.sh` started `llama-server` (installed at /usr/bin) — `--check` OK,
    model loaded, listening on http://127.0.0.1:8080.
  - Gated test `live_smoke_against_llama_server` (in `bge_m3.rs`, `#[ignore]`):
    run `cargo test --lib embed::bge_m3::tests::live_smoke_against_llama_server -- --ignored`.
    Result: **1024 dims, L2 unit-norm** ✓; cosine(register user, create account)=**0.78**,
    cosine(register user, quantum chromo …)=0.28.
  - **Calibration note for T7.2:** the real BGE-M3 near-pair score is **0.78, below the
    fixture-derived semantic_match_threshold (0.85)**. The 0.85 fixture contract is about
    `FixtureEmbedder`; do NOT assume BGE-M3 reaches it on the the same text pair. T7.2 should
    calibrate its merge threshold against the live embedder's distribution, or accept that
    some paraphrases won't merge — do not edit the fixture contract to match BGE-M3.
    **UPDATED (2026-08-11): this note is superseded — see the live-calibration result below.**
- **Live BGE-M3 calibration probe (2026-08-11) — IMPORTANT design evidence for T7.2:**
  `tests/live_calibration.rs` (gated `#[ignore]`; run `cargo test --test live_calibration --
  --ignored --nocapture` against a running server). Findings:
  1. **Bare concept-name embedding does NOT separate the classes.** On 8 curated should-merge
     vs 8 must-not-merge 2-3 word labels: near range [0.567, 0.868], far range [0.429, 0.855].
     **They overlap** (far-max 0.855 > near-min 0.567) so NO single threshold works on bare
     labels. Dense vectors of short noun-phrases are noisy; antonym/domain-twin pairs
     (reset/forgot 0.86, delete/create user 0.76) embed nearer than true paraphrases
     (deploy/ship 0.57, charge/process 0.62).
  2. **Embedding WITH sentence context fixes it.** Same concepts inside short sentences:
     near = [0.867, 0.931], far = [0.750, 0.825] — a clean gap (0.825, 0.867). **The
     existing 0.85 threshold sits inside that gap.**
  **=> Rule for T7.2:** hybrid matching must embed the concept WITH context (name + origin
  interaction text), never the bare label. Keep 0.85 as default; make it configurable and
  re-calibrate per embedder on a larger labeled set (precision/recall) before shipping.
  Bias toward precision (under-merge = separate concepts, safe for canonization) over
  over-merge. The demo's 'one hybrid merge' is achievable via context embedding.
- **P7 review remediation (2026-08-11, per `dev-diary/adversarial-review/adve-review-p7-embeddings.md`):**
  - **R2 dim:** v0.1 now fails fast unless dim == 1024 (Cockroach `VECTOR(1024)`): single guard in
    `build_embedder` for all kinds + defense-in-depth in `BgeM3LlamaCppEmbedder::new`. Fixture
    branch no longer needs its own check.
  - **R5 env split:** `LAMBO_LLAMA_MODEL` is the only env feeding the HTTP `/v1/embeddings` model
    id. `LAMBO_BGE_M3_MODEL` is now documented as the GGUF *filesystem path* only (scripts); the
    old dual-feed fallback (`or_else(LAMBO_BGE_M3_MODEL)`) was removed so a path is never sent
    as a model id. `.env.example` updated.
  - **R6 repro:** `scripts/fetch-bge-m3.sh` now pins `LAMBO_BGE_M3_REVISION`
    (default `2d48f173...`) and passes `--revision` to hf/huggingface-cli; curl fallback uses the
    revision in the URL instead of `main`.
  - **R14/R15:** live `#[ignore]` smoke (bge_m3.rs) + live calibration probe (tests/) committed;
    duplicate `## Handoff Log` headers merged.
- **Announcement (shared Cargo.toml exception):** added additive deps without a separate claim —
  `reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }`
  (rustls to match sqlx) and dev dep `httpmock = "0.7"` for the HTTP client tests.
- **Surprises / gotchas:**
  - httpmock `json_body_partial` takes a JSON *string* (not `serde_json::Value`) — write the body
    as a `r#"..."#` literal.
  - llama.cpp `/v1/embeddings` model field: some servers 400 on a model id that isn't loaded, so
    the request omits `model` when `LAMBO_LLAMA_MODEL` is empty (server default). Documented for the
    demo machine: set `LAMBO_LLAMA_MODEL` to the served model name if the default check fails.
- **Next agent should not re-derive:** the exact request/response shape and the normalization step
  are already implemented and mocked-tested. T7.2/T7.3 only need a working `Embedder`; call
  `embedder_from_env()` or `build_embedder(cfg)`.
- **To reproduce offline:** `cargo test embed::bge_m3` (httpmock, no model/server needed).
- **Live smoke pending (ops checklist):** model weights not downloaded here (several GB, gitignored).
  Follow `notes/embeddings-portable.md` ops checklist: `./scripts/fetch-bge-m3.sh` then
  `./scripts/run-llama-embed.sh`.
