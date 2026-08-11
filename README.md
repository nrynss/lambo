# Lambo

Agentic memory for multi-agent coding: a bipartite interaction/concept graph with
write-behind durability (CockroachDB), background scoring/GC, and **canonization**
(importance earned from structural evidence, not declared at write time).

## Deployment model (single writer)

**One `lambo serve` process owns a session.** Agents connect over MCP (stdio or HTTP)
and share that process's in-memory graph. Mutations flush asynchronously to the durable
store. Any number of **readers** may query the store directly (dashboards, CockroachDB
managed MCP) and see eventually consistent state. Readers never write. Multi-writer
coordination is out of scope for v0.1.

## Status

Hackathon build (CockroachDB × AWS). Spec: [`lambo-hackathon-spec-v0.1.md`](lambo-hackathon-spec-v0.1.md).
Phase handoffs: [`dev-diary/`](dev-diary/).

```bash
# prerequisites: Rust stable; optional .env (see .env.example)
cargo run -- --help
cargo test
```

## Pluggable backends (Level B)

Adapters are **feature-gated** and **config-selected** (not dynlib plugins):

| Layer | Cargo features | Select via |
|-------|----------------|------------|
| Store | `store-memory` (default), `store-cockroach`, `store-sqlite` | `[store]` in `lambo.toml` / `LAMBO_STORE` |
| Embedder | `embed-bge` (default), `embed-fixture`, `embed-bedrock` | `[embedder]` / `LAMBO_EMBEDDER` |

Process start uses **`resolve_from_config_path` / `resolve_backends`** once
(`ResolvedBackends`) — store + embedder + dim compatibility + `EmbeddingContract`. Commands
must consume that result, not rebuild backends.

Design of record: [`dev-diary/notes/level-b-pluggability.md`](dev-diary/notes/level-b-pluggability.md).  
Example file: [`lambo.example.toml`](lambo.example.toml).

```bash
# default = memory store + BGE + fixture embedder + fixtures
cargo test

# ship / demo profile (when CockroachStore lands in T3.2)
cargo build --release --features demo
```

### Vector width (not hardwired)

- Config `embedder.dim` is the embedder’s expected output width (default **1024** for BGE demos).
- Stores that **persist** vectors expose `GraphStore::vector_dimensions() -> Some(n)`
  (Cockroach `VECTOR(n)` when T3.2 lands). MemoryStore returns `None` (no column constraint).
- Resolution fails if a store width and embedder width disagree.

## Embeddings (portable)

Embeddings sit behind an **`Embedder` trait** (dense, L2-normalized). Backends:

| Backend | Feature | Role |
|---------|---------|------|
| **BGE-M3** via **Hugging Face** + **llama.cpp** | `embed-bge` | **Default** while Bedrock is unavailable |
| **Amazon Titan Text Embeddings V2** (Bedrock) | `embed-bedrock` | Swap-in when account is authorized (T7.1) |
| **FixtureEmbedder** | `embed-fixture` | Unit tests / CI only |

**Do not mix models** in one session without re-embedding (same dim ≠ same space). Session
identity is `EmbeddingContract { kind, model, dim }` on the graph snapshot.

Setup notes: [`dev-diary/notes/embeddings-portable.md`](dev-diary/notes/embeddings-portable.md).  
Bedrock account gate: [`dev-diary/notes/bedrock-authorization-blocker.md`](dev-diary/notes/bedrock-authorization-blocker.md).

```bash
# high level
# 1) huggingface-cli download … → models/bge-m3/*.gguf  (gitignored)
# 2) llama-server -m … --embedding --port 8080
# 3) LAMBO_EMBEDDER=bge_m3 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080
#    or copy lambo.example.toml → lambo.toml
```

## License

MIT — see [LICENSE](LICENSE).

Derived from design work credited as Lambo v0.6.0 (prior design, not incorporated code).
