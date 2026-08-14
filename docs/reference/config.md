# Configuration & deployment reference

> Verified against `src/config.rs`, `Cargo.toml`, `scripts/provision.sh`.

## `lambo.toml`

Process config. Resolution order: `--config <PATH>` → `LAMBO_CONFIG` env →
`./lambo.toml`. **Env always wins over file** for keys it sets (Level B).

### `[store]`
```toml
[store]
kind = "memory"   # "memory" | "sqlite" | "cockroach"
# dsn / path as required by the kind
```
The store kind must be compiled in (see [Features](#features)); an
un-compiled kind fails closed with a clear message.

### `[embedder]`
```toml
[embedder]
kind = "fixture"  # "fixture" | "bge" | "bedrock"
dim  = 1024
```
Do **not** switch embedder models mid-session without re-embedding — vectors from
different models are not comparable.

### Tunables (selected)
| Key | Meaning |
|---|---|
| `backend_flush_interval` | Write-behind flush cadence (the durability bound). |
| `backend_flush_max_batch`, `backend_flush_retries`, `backend_log_max` | Flush batching / retry / log cap. |
| `daemon_tick_interval` | Daemon cadence. |
| `default_top_k`, `default_max_tokens`, `default_traversal_depth` | Recall defaults (clamped to the MCP maxima at the surface). |
| `match_strategy` | `Canonical` (default) \| `Hybrid`. |
| `max_canonical_nodes`, `canonization_*` | Canonization ("saints") thresholds and cadence. |
| `scoring` / `recall_weights` | Scoring and recall weight vectors. |

Full key list and defaults: `src/config.rs` (`Config` struct) and
`lambo.example.toml`.

## Features

`default = ["store-memory", "embed-bge", "embed-fixture", "fixtures"]`

| Feature | Pulls in |
|---|---|
| `store-memory` | in-RAM store (also required by `fixtures`) |
| `store-sqlite` | `SqliteStore` (`sqlx` + sqlite) |
| `store-cockroach` | `CockroachStore` (`sqlx` + postgres) |
| `embed-fixture` | deterministic fixture embedder |
| `embed-bge` | BGE-M3 over a llama.cpp endpoint (`reqwest`) |
| `embed-bedrock` | Bedrock Titan (authorization-gated) |
| `fixtures` | committed fixture graphs for tests |
| `demo` | ship profile: `store-memory,store-cockroach,embed-bge,embed-fixture,fixtures` |

Build for the demo/ship profile:
```bash
cargo build --release --features demo
```

## HTTP transport hardening (pending — T8.7)

The streamable-HTTP transport is loopback-default and **currently
unauthenticated, unrate-limited, with unbounded session creation**. Auth on
non-loopback bind, a rate limit, and a concurrent-session cap are tracked in
**T8.7** — do not expose the HTTP transport off loopback until that lands.

## Provisioning

`scripts/provision.sh` applies the durable store schema (including the T8.6
`session_leases` table). `lambo provision` (the in-binary wrapper) is part of
**T8.3** and not yet wired — use `scripts/provision.sh` for now.

See also: [api.md](api.md), [mcp.md](mcp.md).
