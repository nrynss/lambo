# Portable embeddings — BGE-M3 (default) + Bedrock swap-in

**Decision (2026-08-10):** Embeddings are a **pluggable** layer behind the `Embedder` trait.
Default production path while Bedrock is blocked: **BAAI/bge-m3** downloaded from
**Hugging Face**, served with **llama.cpp**. Swap to **Amazon Titan Text Embeddings V2** on
Bedrock when `authorizationStatus` is `AUTHORIZED`.

**Packaging (2026-08-11):** Level B — Cargo features (`embed-bge` / `embed-fixture` /
`embed-bedrock`) + TOML/env selection + `resolve_backends`. See
[level-b-pluggability.md](level-b-pluggability.md).

**Do not** mix vectors from different models in one session/index without re-embedding.
Same dimension does **not** mean the same embedding space. Enforce with
`EmbeddingContract { kind, model, dim }` on `GraphSnapshot` (helpers in `src/resolve.rs`).

**Dim:** config default 1024 suits BGE + Cockroach demo DDL. It is **not** hardwired in
`build_embedder`. Stores that persist vectors declare width via
`GraphStore::vector_dimensions()`.

---

## Why BGE-M3

| Property | Titan V2 (Bedrock) | BGE-M3 (default now) |
|----------|--------------------|----------------------|
| Dense dim | 1024 (default) | **1024** — matches `VECTOR(1024)` / T0.3 spike |
| Context | ~8192 tokens | ~8192 tokens |
| Multilingual | English + 100+ (preview) | Strong cross-lingual dense retrieval |
| Hosting | Bedrock (blocked on this account) | Local via HF + llama.cpp |
| Extra modes | Dense only | Dense (+ sparse / multi-vector unused in v0.1) |

v0.1 uses **dense embeddings only** for hybrid concept matching (spec §7.1 step 6) and
`vector_candidates`. Sparse/ColBERT paths are out of scope.

---

## Architecture

```text
                    ┌──────────────────────────┐
  derive / hybrid → │  Embedder trait          │  dimensions() + embed(text) -> Vec<f32>
                    └────────────┬─────────────┘
         ┌───────────────────────┼───────────────────────┐
         ▼                       ▼                       ▼
  BgeM3LlamaCppEmbedder    BedrockEmbedder         FixtureEmbedder
  (default)                (when authorized)       (unit tests only)
  HF weights + llama.cpp   Titan V2 1024-dim       deterministic 1024-d
```

| Backend | Env `LAMBO_EMBEDDER` | Dim | When |
|---------|----------------------|-----|------|
| BGE-M3 via llama.cpp | `bge_m3` (default) | 1024 | Always available offline after model download |
| Bedrock Titan V2 | `bedrock` | 1024 | Account `authorizationStatus: AUTHORIZED` |
| Fixture | `fixture` | 1024 | Tests / CI without models |

Schema stays **`VECTOR(1024)`**. Normalize embeddings (L2) before store/query so Cockroach
`<->` (L2) rankings stay coherent (Titan used `normalize: true`).

Config / capability:

- Advertise `Capabilities::VECTOR_SEARCH` only when an embedder is configured and live.
- One active embedder per process; changing backend requires re-embed or new session.

---

## Runtime layout

```text
lambo/
  models/                    # gitignored — HF download target
    bge-m3/                  # or GGUF path used by llama.cpp
  scripts/
    fetch-bge-m3.sh          # HF download (to implement / document)
    run-llama-embed.sh       # start llama.cpp embedding server (to implement)
```

Never commit model weights. Paths overridable via env.

---

## Setup: Hugging Face download + llama.cpp

### Prerequisites

- `git` / [`huggingface-cli`](https://huggingface.co/docs/huggingface_hub) (`pip install huggingface_hub`)
- [llama.cpp](https://github.com/ggerganov/llama.cpp) built with embedding support
- Disk: BGE-M3 GGUF variants vary; plan several GB free

### 1. Download weights from Hugging Face

Preferred: a **GGUF** build of BGE-M3 suitable for llama.cpp (community or official
conversion). Example pattern (adjust repo/filename to the GGUF you choose):

```bash
# Create local model dir (gitignored)
mkdir -p models/bge-m3
cd models/bge-m3

# Option A — huggingface-cli
huggingface-cli download <org>/<bge-m3-gguf-repo> \
  --include "*.gguf" \
  --local-dir .

# Option B — git LFS
# git lfs install
# git clone https://huggingface.co/<org>/<bge-m3-gguf-repo>
```

Record the exact HF repo + revision in the Handoff Log when scripts land so demos are
reproducible.

**Original model card (dense reference):** [BAAI/bge-m3](https://huggingface.co/BAAI/bge-m3)

### 2. Run embeddings with llama.cpp

llama.cpp can expose embeddings via its server (or CLI). Typical server pattern:

```bash
# Example — flags vary by llama.cpp version; verify with --help
./llama-server \
  -m /path/to/bge-m3-*.gguf \
  --host 127.0.0.1 \
  --port 8080 \
  --embedding
```

Lambo talks to the local server over HTTP (OpenAI-compatible or llama.cpp native embed
endpoint — finalize in implementation and document the chosen path here).

```bash
# Health check (example; adjust path to your llama.cpp API)
curl -s http://127.0.0.1:8080/health
```

### 3. Point Lambo at the server

```bash
# .env
LAMBO_EMBEDDER=bge_m3
LAMBO_EMBED_DIM=1024
LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080
# optional: LAMBO_BGE_M3_MODEL=/abs/path/to/model.gguf  (if lambo spawns llama.cpp)
```

Then hybrid matching and `vector_candidates` use 1024-d dense vectors in Cockroach.

---

## Bedrock swap-in (when authorized)

See also [`bedrock-authorization-blocker.md`](bedrock-authorization-blocker.md).

When availability is `AUTHORIZED`:

```bash
LAMBO_EMBEDDER=bedrock
LAMBO_BEDROCK_REGION=us-east-1   # or ap-south-2 when unlocked there
LAMBO_EMBED_DIM=1024
# aws login  OR  AWS_BEARER_TOKEN_BEDROCK=...
```

Model id: `amazon.titan-embed-text-v2:0`  
Request shape: `{"inputText":"...","dimensions":1024,"normalize":true}`

**Migration rule:** do not append Titan vectors into a graph already filled with BGE-M3
vectors (or the reverse). Start a new session or re-embed all concepts.

---

## Implementation map (P7)

| Task | Owns | Notes |
|------|------|--------|
| T1.3 (done) | `FixtureEmbedder` | Tests / near-far contract |
| T7.x | `src/embed/mod.rs` | Trait + factory from `LAMBO_EMBEDDER` |
| T7.x | `src/embed/bge_m3.rs` (or `llama_cpp.rs`) | HTTP client to llama.cpp |
| T7.1 | `src/embed/bedrock.rs` | Optional; gated on auth |
| T7.3 | Cockroach vector path | Unchanged dim 1024; EXPLAIN index use |
| Scripts | `scripts/fetch-bge-m3.sh`, `scripts/run-llama-embed.sh` | Reproducible demo ops |

**Degradation:** if embedder/server is down, hybrid falls back to canonical matching and
logs once — not keyword-as-product-story. Prefer fail-visible for demo if BGE-M3 is the
declared path.

---

## Ops checklist (demo machine)

- [ ] HF download complete under `models/` (gitignored)
- [ ] llama.cpp embedding server running on `LAMBO_LLAMA_EMBED_URL`
- [ ] `LAMBO_EMBEDDER=bge_m3` and dim 1024
- [ ] Cockroach schema has `VECTOR(1024)` + vector index (T0.2 / T0.3)
- [ ] Smoke: embed one string → 1024 floats → insert/query via store
- [ ] Optional: Bedrock path documented for after AWS unlock

---

## EmbeddingGemma 2 on llama.cpp (#22, text and images)

**Decision (2026-10-09, #22 PR 5):** EmbeddingGemma 2 (`kind = "embeddinggemma2"`,
feature `embed-eg2`) is served by the same `llama-server` Lambo already talks to, for
text and images in one space. Decisions: `feature-22-image-embeddings.md`; live
evidence: `evidence/issue-22-eg2/`.

- **Version floor: llama.cpp b11452** (PR #30054 added the `gemma-embedding2`
  architecture). Older builds refuse with `unknown model architecture:
  'gemma-embedding2'`. Verified on the upstream release **b11517**. Homebrew's stable
  formula was pinned to b11429 when this was written, and its `--HEAD` build failed
  until ggml-org/ggml syncs `ggml_backend_sched_set_copy_callback`, so use an upstream
  release binary (or a source build) until Homebrew catches up.
- **Weights:** `ggml-org/embeddinggemma-2-GGUF` at revision `bfcd2987`, converted from
  `google/embeddinggemma-2` at `914f7f89`: `embeddinggemma-2-Q8_0.gguf` (310 MB, sha256
  `2188ac1d…`) and the vision and audio projector `mmproj-embeddinggemma-2-Q8_0.gguf`
  (555 MB, `c4a8a526…`). Outside the repo, like every model.
- **Server:**

  ```bash
  llama-server --host 127.0.0.1 --port 8191 \
    -m embeddinggemma-2-Q8_0.gguf --mmproj mmproj-embeddinggemma-2-Q8_0.gguf \
    --embeddings --pooling mean \
    --image-min-tokens 280 --image-max-tokens 280 \
    --ctx-size 8192 --batch-size 8192 --ubatch-size 8192
  ```

  The image flags fix the 280-token budget of the `lambo-eg2-v2` profile (the server's
  default sizes images at 85 to 125 tokens, and the budget changes the vectors). The
  batch flags let a whole image fit one ubatch: at the default 512 the server caps the
  budget to 256 without failing, and Lambo then refuses every image. Leave out
  `--mmproj` for a text-only server and set `[embedder] images = false`.
- **What Lambo does, not the server:** the task prefixes (`title: none | text: ` for
  stored text, `task: search result | query: ` for recall queries, none for images),
  the canonical image form (every image decoded and resized to a 768 px longer side,
  sent as lossless PNG), MRL truncation to `dim` (768, 512, 256 or 128)
  with re-normalization, and the contract string `<artifact>;prompts=lambo-eg2-v2`.
- **Startup check:** before its first embed Lambo reads `/props` (`model_path`,
  `model_ftype`, `modalities.vision`) and refuses a file that is not EmbeddingGemma 2,
  another quantization than `model` names, or images on a server without a projector.
- **Cost on this Mac (Metal, b11517):** text about 7 ms warm; an image at the 280
  budget about 370 ms server-side.

| Backend | `kind` | Dim | When |
|---------|--------|-----|------|
| EmbeddingGemma 2 via llama.cpp | `embeddinggemma2` | 768 (MRL 512/256/128) | Text and images in one space; llama.cpp b11452+ |

---

## Handoff

- Default embedder for development and demo: **BGE-M3 + llama.cpp** (HF weights).
- Bedrock Titan remains the **AWS-native** backend for when account authorization lands.
- Fixture embedder remains for CI without models or network.
