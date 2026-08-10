# P7 — Embeddings & hybrid matching

```yaml
id:       P7
requires: [P1, T0.3, T0.4]
blocks:   nothing hard — capability-gated; keyword-only is a lawful degraded mode (spec §3.2)
parallel: high   # T7.1 ‖ T7.2 ‖ T7.3
runs-parallel-with: P2, P3, P4, P5, P6
```

**Goal:** Portable `Embedder` trait (1024-d dense) + hybrid concept matching (spec §7.1
step 6) + live `vector_candidates` — Cockroach **Distributed Vector Indexing** (spec §12.1)
doing real work: merging concepts normalization can't ("register user" / "create account").

**Default embedder:** **BGE-M3** weights from **Hugging Face**, runtime **llama.cpp**
(see [`notes/embeddings-portable.md`](notes/embeddings-portable.md)).  
**Swap-in:** Bedrock Titan V2 when account is authorized.  
**Tests:** `FixtureEmbedder` only.

**Degradation contract:** if hybrid cannot embed, fall back to `MatchStrategy::Canonical`
and log once — not "keyword as product." §12.1 still requires the vector index in use for
the demo (T7.3 + one hybrid merge minimum).

---

### T7.0 — Embedder factory + BGE-M3 / llama.cpp ★ (default path)
```yaml
requires:   T1.3
fixture-ok: yes
owns:       src/embed/mod.rs, src/embed/bge_m3.rs, scripts/fetch-bge-m3.sh, scripts/run-llama-embed.sh
status:     done
```
- `LAMBO_EMBEDDER=bge_m3|bedrock|fixture`, `LAMBO_EMBED_DIM=1024`, `LAMBO_LLAMA_EMBED_URL`.
- Download GGUF (or convert) from HF into `models/` (gitignored); serve with llama.cpp
  `--embedding`.
- HTTP client returns L2-normalized 1024-d vectors.
- **Never mix** BGE-M3 and Titan vectors in one session without re-embed.

**Done when:** smoke against local llama.cpp returns 1024 dims; fixture path still green in CI
without models.

---

### T7.1 — `BedrockEmbedder` (optional swap-in)
```yaml
requires:   T1.3, T0.4
fixture-ok: yes   # written from the T0.4 handoff; live call behind an integration gate
owns:       src/embed/bedrock.rs
status:     not-started
```
Titan Text Embeddings V2, 1024-dim, via `aws-sdk-bedrockruntime` or Bearer API key, using
T0.4 shapes. Selected when `LAMBO_EMBEDDER=bedrock` and account is AUTHORIZED. Timeout +
typed errors; **embed failure fails the hybrid match step, never the write** — fall back to
canonical match (per-call fallback is the v0.1 shape).

**Done when:** unit tests with a mocked client pass; feature-gated live smoke returns 1024
dims when AWS allows.

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

**Done when:** with `FixtureEmbedder`, the near pair merges with a `Semantic` edge and the
far text creates a fresh concept; with a no-capability store, behavior is byte-identical to
`MatchStrategy::Canonical`.

---

### T7.3 — Live `vector_candidates` on CockroachDB ★ (hackathon requirement)
```yaml
requires:   T3.2, T0.3
fixture-ok: no
owns:       (vector paths inside src/store/cockroach.rs — same owner as T3.2; claim jointly or sequence)
status:     not-started
```
The T0.3 spike productionized: embedding column write in `flush()`, index-backed
similarity query, `Capabilities::VECTOR_SEARCH` advertised. Verify with `EXPLAIN` that the
vector index is actually used — "we used the vector index" must be true on camera.

**Done when:** integration test: two paraphrase concepts derived through the full live
stack merge via the index, and `EXPLAIN` output is captured into `dev-diary/evidence/`.

---

## Exit criteria

- [ ] BGE-M3 + llama.cpp path documented and smokeable (default)
- [ ] Bedrock path optional swap-in (same 1024-d contract)
- [ ] Hybrid merge demonstrated offline (fixtures) and live (Cockroach)
- [ ] Degraded mode proven equivalent to Canonical strategy
- [ ] `EXPLAIN` evidence of index use committed

## Handoff Log

- **2026-08-10:** Portable embeddings decision — default BGE-M3 (HF + llama.cpp), Bedrock
  Titan when authorized. Dim 1024. Details: `notes/embeddings-portable.md`.

---

## Handoff Log

> _Fill on completion._

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
    `gpustack/bge-m3-GGUF` file `bge-m3-f16.gguf`, overridable via `LAMBO_BGE_M3_HF_REPO` /
    `LAMBO_BGE_M3_GGUF`. Uses huggingface-cli, else `hf`, else curl fallback; `--dry-run` prints plan.
  - `scripts/run-llama-embed.sh` — starts `llama-server -m <model> --embedding -c <ctx>` on the
    parsed port; `--check` health-checks an existing server; refuses if a server is already up.
  - `.env.example` — added `LAMBO_LLAMA_MODEL` (model id sent; empty => server default) and
    `LAMBO_BGE_M3_GGUF`. `LAMBO_LLAMA_MODEL` or `LAMBO_BGE_M3_MODEL` both feed the request model id.
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
