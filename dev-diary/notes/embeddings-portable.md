# Portable embeddings — BGE-M3 (default) + Bedrock swap-in

**Decision (2026-08-10):** Embeddings are a **pluggable** layer behind the `Embedder` trait.
Default production path while Bedrock is blocked: **BAAI/bge-m3** downloaded from
**Hugging Face**, served with **llama.cpp**. Swap to **Amazon Titan Text Embeddings V2** on
Bedrock when `authorizationStatus` is `AUTHORIZED`.

**Packaging (2026-08-11):** Level B — Cargo features (`embed-bge` / `embed-fixture` /
`embed-bedrock`) + TOML/env selection. See [level-b-pluggability.md](level-b-pluggability.md).

**Do not** mix vectors from different models in one session/index without re-embedding.
Same dimension (1024) does **not** mean the same embedding space.

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

## Handoff

- Default embedder for development and demo: **BGE-M3 + llama.cpp** (HF weights).
- Bedrock Titan remains the **AWS-native** backend for when account authorization lands.
- Fixture embedder remains for CI without models or network.
