# Level B pluggability — design of record

**Decision (2026-08-11):** Lambo backends are **Level B** pluggable:

1. **Cargo features** decide which adapters are *compiled in*.
2. **`lambo.toml` (or env)** selects among those adapters at process start.
3. **Traits** stay the only contract (`GraphStore`, `Embedder`).

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

**Demo / ship binary (P9):** enable `store-cockroach` (and `embed-bedrock` when unlocked):

```bash
cargo build --release --features store-cockroach
# or: --features "store-cockroach,embed-bedrock"
```

**Rule:** selecting a `kind` that was not compiled in is a **hard startup error** with a
rebuild hint (`--features store-cockroach`), never a silent fallback.

---

## Config selection (`lambo.toml`)

Example: [`../../lambo.example.toml`](../../lambo.example.toml).

```toml
[store]
kind = "memory"          # memory | cockroach | sqlite
# dsn = "postgresql://..."
# path = "./lambo.db"    # sqlite

[embedder]
kind = "bge_m3"          # bge_m3 | bedrock | fixture
dim = 1024
url = "http://127.0.0.1:8080"
# model = ""             # llama.cpp model id (optional)
```

**Precedence (highest wins):**

1. Explicit CLI flags (when added on `serve`)
2. Environment variables (`LAMBO_STORE`, `LAMBO_EMBEDDER`, DSNs, …)
3. `lambo.toml` (path: `--config` / `LAMBO_CONFIG` / `./lambo.toml`)
4. Built-in defaults (`store=memory`, `embedder=bge_m3`, `dim=1024`)

Env remains valid for secrets and CI; TOML is the durable operator surface.

---

## Registries (code shape)

```text
StoreKind  ──cfg feature──▶  build_store(StoreConfig)  -> Box<dyn GraphStore>
EmbedderKind ──cfg feature──▶  build_embedder(EmbedderConfig) -> Box<dyn Embedder>
```

- Core graph / daemon / recall / MCP depend only on the traits.
- Each adapter owns its SQL, HTTP, or SDK details (spec §3.1).
- Adding a backend:

  1. `impl GraphStore` or `impl Embedder` in a gated module  
  2. Cargo feature (+ optional deps)  
  3. One arm in the registry  
  4. Document `kind` string in this note + `lambo.example.toml`

No dynamic loading. No plugin ABI. No third-party `.so`.

---

## Invariants that survive any backend

1. **Trait vocabulary only** upward — no dialect leaks (spec §3.1).  
2. **Embedding dim = 1024** in v0.1 (Cockroach `VECTOR(1024)`).  
3. **Do not mix embedder model spaces** in one session without re-embed.  
4. **`Capabilities::VECTOR_SEARCH`** only when the *store* can ANN-query; hybrid degrades otherwise.  
5. **Blast radius** counts structural concept edges only (spec §4.1 errata).  
6. **Single-writer** `lambo serve` per session (spec §2.2) — packaging does not change this.

---

## Sustainability claim

With Level B:

- Offline / fixture-ok tracks never link Cockroach or AWS.  
- Hackathon demo enables Cockroach without forcing it on every `cargo test`.  
- Post-hackathon: SQLite, Dolt, or another embedder is a **feature + adapter**, not a fork.  
- Operators change `lambo.toml` among *built* kinds without recompiling; *new* kinds require a rebuild with the feature (honest and cheap).

Level C (sidecars / dynlibs) is explicitly **out of v0.1 scope**. Revisit only if external
third-party adapters become a product requirement.

---

## Task ownership (binding)

| Work | Owner | Status |
|------|--------|--------|
| Feature matrix + registries + `lambo.toml` schema | **T1.5** | **done** |
| Spec §3.3–§3.4 / §6.1–§6.3 Level B text | errata 2026-08-11 | done |
| `CockroachStore` under `store-cockroach` | **T3.2** | not-started |
| `SqliteStore` under `store-sqlite` | **T3.3** | not-started |
| Structural queries honor §4.1 blast errata | **T3.6** | not-started |
| BGE under `embed-bge` | **T7.0** | done |
| Bedrock under `embed-bedrock` | **T7.1** | not-started |
| `Memory` builder takes `dyn` store/embedder | **T8.1** | not-started |
| `serve` / CLI: `--config`, load `LamboFile`, registries | **T8.2**, **T8.3** | not-started |
| Demo under `--features demo` | **T8.4** | not-started |
| README documents features + example toml | **T9.1** | not-started |
| Architecture diagram shows Level B fan-out | **T9.2** | not-started |

---

## Anti-patterns

- Putting `sqlx` / AWS SDK in the default feature set “just in case.”  
- Silent fallback when TOML asks for `cockroach` but feature is off.  
- Mixing env-only secrets into committed `lambo.toml` (use env for DSN passwords).  
- Treating dim or model id as free-form without session metadata (T7.2 / store column).
