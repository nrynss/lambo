# Level B pluggability — design of record

**Decision (2026-08-11):** Lambo backends are **Level B** pluggable:

1. **Cargo features** decide which adapters are *compiled in*.
2. **`lambo.toml` (or env)** selects among those adapters at process start.
3. **Traits** stay the only contract (`GraphStore`, `Embedder`).
4. **Process resolution** (`src/resolve.rs`) is the **single construction site** for
   store + embedder and the place store×embedder compatibility is checked.

This is the sustainability model: new stores and embedders are **adapter modules + one
registry arm + a feature flag**, not core rewrites and not dynlib/sidecar plugins (Level C).

Related: [embeddings-portable.md](embeddings-portable.md) (model portability rules).

---

## Why Level B

| Level | Meaning | Verdict for Lambo |
|-------|---------|-------------------|
| A | Everything always linked; config only picks | Heavy default binary |
| **B** | **Feature-gated inclusion + TOML/env selection** | **Chosen** |
| C | Runtime `.so` / external process plugins | Deferred (v0.7+ if needed) |

Level B keeps CI and offline tracks lean (`store-memory` + `embed-fixture`), ships the
demo with `store-cockroach` + `embed-bge`, and leaves Bedrock optional until authorized.

---

## Feature matrix

| Feature | Compiles | Default |
|---------|----------|---------|
| `store-memory` | `MemoryStore` | **yes** |
| `store-cockroach` | `CockroachStore` (P3) + `sqlx` postgres | no until T3.2 |
| `store-sqlite` | `SqliteStore` (P3) + `sqlx` sqlite | no until T3.3 |
| `embed-fixture` | `FixtureEmbedder` | **yes** |
| `embed-bge` | `BgeM3LlamaCppEmbedder` + `reqwest` | **yes** |
| `embed-bedrock` | Bedrock Titan (T7.1) + AWS SDK | no until authorized |
| `fixtures` | T1.4 JSON loader (implies `store-memory`) | **yes** |

**Default feature set:**

```text
store-memory + embed-bge + embed-fixture + fixtures
```

**Demo / ship binary (P9):**

```bash
cargo build --release --features demo
# or: --features "store-cockroach,embed-bge,…"
```

**Rule:** selecting a `kind` that was not compiled in is a **hard startup error** with a
rebuild hint (`--features store-cockroach`), never a silent fallback.

Unknown TOML keys are also hard errors (`#[serde(deny_unknown_fields)]` on `LamboFile`,
`StoreConfig`, `EmbedderConfig`) so typos like `knd` / `[embeder]` do not silently default.

---

## Config selection (`lambo.toml`)

Example: [`../../lambo.example.toml`](../../lambo.example.toml).

```toml
[store]
kind = "memory"          # memory | cockroach | sqlite
# dsn = "postgresql://..."   # secrets: prefer env LAMBO_COCKROACH_DSN
# path = "./lambo.db"        # sqlite

[embedder]
kind = "bge_m3"          # bge_m3 | bedrock | fixture
dim = 1024               # expected embedder width (default for BGE demos — not a global law)
url = "http://127.0.0.1:8080"
# model = ""             # llama.cpp model id (optional)
```

**Precedence (highest wins):**

1. Explicit CLI flags (when added on `serve`)
2. Environment variables (`LAMBO_STORE`, `LAMBO_EMBEDDER`, DSNs, …) — **non-empty only**
3. `lambo.toml` (path: `--config` / `LAMBO_CONFIG` / `./lambo.toml`)
4. Built-in defaults (`store=memory`, `embedder=bge_m3`, `dim=1024`)

Empty env values (e.g. `LAMBO_COCKROACH_DSN=` in a template `.env`) are treated as **unset**
and leave the file value intact.

---

## Resolution pipeline (single construction site)

```text
LamboFile::load_resolved(path?)
        │
        ▼
resolve_backends(file)          // src/resolve.rs
        │
        ├─ build_store(store_cfg)       → Box<dyn GraphStore>
        ├─ build_embedder(embed_cfg)    → Box<dyn Embedder>
        ├─ check_vector_compatibility(store.vector_dimensions(), embedder.dimensions())
        └─ EmbeddingContract { kind, model, dim }
        │
        ▼
ResolvedBackends { store, embedder, store_cfg, embedder_cfg, embedding }
        │
        ▼
CLI / Memory / serve  ←── consume this value; do NOT rebuild
```

| API | Role |
|-----|------|
| `build_store` / `build_embedder` | Low-level registries (adapter unit tests OK) |
| `resolve_backends` / `resolve_from_config_path` | **Process start** — required for serve/demo |
| `resolve_store_only` | Provision / ops paths that need no embedder |
| `assert_session_embedding_compatible` | On session attach when snapshot already has a contract |

**T8 rule:** `Memory::builder` / `lambo serve` take `ResolvedBackends` (or its fields once).
Never call `build_*` again with a second config pass.

---

## Vector dimensions (not hardwired)

| Layer | Responsibility |
|-------|----------------|
| `GraphStore::vector_dimensions()` | `Some(n)` if the adapter **persists** dense vectors (Cockroach `VECTOR(n)`); `None` if no column (MemoryStore, SQLite without vectors) |
| `Embedder::dimensions()` / `embedder.dim` | What this backend emits (config default **1024** for BGE demos) |
| `resolve::check_vector_compatibility` | If store says `Some(n)`, require embedder dim == n |

**Do not** reject non-1024 in `build_embedder`. Cockroach’s column width is an **adapter
schema fact** (T3.2 implements `vector_dimensions() -> Some(n)`), not a global constant.

Fixture embedder supports `with_dimensions(n)` for tests; default remains 1024 for near/far goldens.

---

## Embedding model space (session contract)

Same dim does **not** mean interchangeable models (BGE ≠ Titan).

```rust
// GraphSnapshot.embedding
EmbeddingContract { kind, model, dim }
```

- Stamp on first embed / session open.
- On re-attach: `ensure_compatible` / `assert_session_embedding_compatible` — refuse kind,
  model, or dim changes without re-embed or a new session.
- Types + helpers land now; **enforce on serve attach + hybrid write in T7.2 / T8.1**.

---

## Registries (code shape)

```text
StoreKind  ──cfg feature──▶  build_store(StoreConfig)  -> Box<dyn GraphStore>
EmbedderKind ──cfg feature──▶  build_embedder(EmbedderConfig) -> Box<dyn Embedder>
```

**Belt-and-braces (do not “simplify” incorrectly):**

- `is_compiled()` is a **message** pre-check so errors say `rebuild with --features X`
  without naming types that are not in the binary.
- The **real gate** is each `#[cfg(feature = "...")]` arm that constructs the adapter.
- Both are required: pre-check alone cannot reference uncompiled types; cfg alone yields a
  poorer error when the arm is missing.

Adding a backend:

1. `impl GraphStore` or `impl Embedder` in a gated module  
2. Cargo feature (+ optional deps)  
3. One registry arm (+ `is_ready` when the impl is complete)  
4. Document `kind` in this note + `lambo.example.toml`  
5. For stores with vectors: implement `vector_dimensions() -> Some(n)`

No dynamic loading. No plugin ABI. No third-party `.so`.

Flat `StoreConfig` / `EmbedderConfig` (side-by-side fields) is intentional for v0.1.
Per-kind tagged configs can wait until adapter #4 (e.g. Dolt).

---

## Invariants that survive any backend

1. **Trait vocabulary only** upward — no dialect leaks (spec §3.1).  
2. **Vector width is store-authoritative** (see above) — not a global 1024 hardcode.  
3. **Do not mix embedder model spaces** in one session without re-embed (`EmbeddingContract`).  
4. **`Capabilities::VECTOR_SEARCH`** only when the *store* can ANN-query; hybrid degrades otherwise.  
5. **Blast radius** counts structural concept edges only (spec §4.1 errata).  
6. **Single-writer** `lambo serve` per session (spec §2.2).  
7. **Single construction site** — `ResolvedBackends` handed into commands.

---

## Code map

| Path | Role |
|------|------|
| `src/store/mod.rs` | `GraphStore`, `StoreKind`, `build_store`, `vector_dimensions` |
| `src/embed/mod.rs` | `Embedder`, `EmbedderKind`, `build_embedder` |
| `src/embed/math.rs` | Ungated `cosine` (any feature set) |
| `src/resolve.rs` | `resolve_backends`, compatibility, session contract helpers |
| `src/config.rs` | `LamboFile`, env overlay, discover path |
| `src/types/mod.rs` | `EmbeddingContract` on `GraphSnapshot` |
| `src/main.rs` | `--config`; constructs `ResolvedBackends` once |
| `src/test_util.rs` | Shared env mutex for lib tests (`#[cfg(test)]`) |

---

## Sustainability claim

With Level B:

- Offline / fixture-ok tracks never link Cockroach or AWS.  
- Hackathon demo enables Cockroach without forcing it on every `cargo test`.  
- Post-hackathon: SQLite, Dolt, or another embedder is a **feature + adapter**, not a fork.  
- Operators change `lambo.toml` among *built* kinds without recompiling; *new* kinds require a rebuild with the feature (honest and cheap).  
- Dim and model identity stay correct when stores/embedders diversify.

Level C (sidecars / dynlibs) is **out of v0.1 scope**.

---

## Task ownership (binding)

| Work | Owner | Status |
|------|--------|--------|
| Feature matrix + registries + `lambo.toml` | **T1.5** | **done** |
| Spec §3.3–§3.4 / §6 Level B + dim errata | spec | **done** |
| Store×embedder dim check + `EmbeddingContract` types | `resolve` + types | **done** (wire on attach: T8.1) |
| `CockroachStore` + `vector_dimensions() -> Some(n)` | **T3.2** | not-started |
| `SqliteStore` | **T3.3** | not-started |
| Structural queries §4.1 blast errata | **T3.6** | not-started |
| BGE under `embed-bge` | **T7.0** | done |
| Bedrock under `embed-bedrock` | **T7.1** | not-started |
| Stamp/check contract on hybrid write | **T7.2** | partial |
| `Memory` takes `ResolvedBackends` | **T8.1** | not-started |
| `serve` consumes resolved backends only | **T8.2**, **T8.3** | partial (CLI once) |
| Demo `--features demo` | **T8.4** | not-started |
| README features + example toml | **T9.1** | partial |
| Architecture diagram Level B fan-out | **T9.2** | not-started |

---

## Anti-patterns

- Putting `sqlx` / AWS SDK in the default feature set “just in case.”  
- Silent fallback when TOML asks for `cockroach` but feature is off.  
- Silent default when TOML keys are misspelled (must `deny_unknown_fields`).  
- Hard-coding 1024 in `build_embedder` / BGE ctor.  
- Rebuilding store/embedder inside each CLI command after `resolve_*`.  
- Mixing embedder models in one session without re-embed.  
- Secrets in committed `lambo.toml` (use env for DSN passwords).  
- Logging `StoreConfig` without redaction (`Debug` redacts `dsn`).
