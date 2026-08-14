# Library / API reference

Both surfaces — MCP tools and CLI verbs — are thin adapters over one type:
`Memory`. This page documents the public library API (T8.1), the Level B adapter
traits, and the single-writer lease (T8.6).

> Verified against `src/memory.rs`, `src/store/mod.rs`, `src/embed/mod.rs`,
> `src/store/lease.rs`. For full signatures, `cargo doc --open`.

## `Memory`

The RAM-tier graph plus its write-behind daemon, flush task, and canonization
task. Exactly one `Memory` writes a given session (see [Lease](#single-writer-lease)).

### Construction — `MemoryBuilder`

```rust
let mem = Memory::builder()
    .session("my-session")
    .agent("agent-a")
    .backends(resolved)          // preferred: one ResolvedBackends (store + embedder + contract)
    .build()
    .await?;
```

Builder setters: `session`, `agent`, `store`, `embedder`, `embedding_contract`,
`backends`, `match_strategy`, `flush_interval`, `scoring_weights`, `config`.
Prefer `.backends(ResolvedBackends)` over wiring `store`/`embedder` separately —
it is the single construction site (Level B) and avoids double-construction.

`build().await` acquires the writer lease (fails closed if held), runs the
startup load, and spawns the daemon + flush + canonization tasks.

### Accessors
`session()`, `agent()`, `config()`, `embedding_contract()`, `graph()`, `index()`, `store()`.

### Writes
| Method | Purpose |
|---|---|
| `derive(...)` (async) | Derive concepts from an interaction (spec §7). |
| `record_action(&Action)` | Record an action → a Resource concept + Causal/Dependency edges. |
| `demote(chunk, group)` | Demote a chunk. |
| `retract(target, DryRun)` (async) | Retract a concept; `DryRun` returns an `ImpactReport` without mutating. |
| `reserve(node, ttl)` / `release(node)` | Advisory soft lock (RAM-local). |
| `set_root_goal(&[...])`, `declare_synonym(src, canon)` | Session bootstrap. |

### Reads
| Method | Returns |
|---|---|
| `recall(RecallQuery)` (async) | `RecallResult` — the context block. |
| `canonical_memories()` | `Vec<CanonicalMemory>` — the "saints". |
| `stats()` | `MemoryStats` — flush lag, log depth, counts, degraded. |
| `events()` | `broadcast::Receiver<DaemonEvent>` — live event feed. |

### Shutdown
`close().await` — flushes the write-behind tail and releases the lease. Bounded
by `CLOSE_GRACE`; on a hung dependency it abandons the tail (which is then lost —
there is no on-disk WAL) rather than hang forever, and says so honestly.

## Level B adapter traits

Backends are selected by cargo feature → registry → `dyn Trait`. See
[config.md](config.md#features) for the feature flags.

### `GraphStore` (`src/store/mod.rs`)
Durable graph persistence. Key methods: `init_schema`, `flush(&MutationBatch)`,
`load_session(&SessionId)`, `keyword_candidates`, `vector_candidates`,
`blast_radius`, `interaction_span`, `record_canonization`, plus the lease methods
below. Implementations: `MemoryStore` (`store-memory`), `SqliteStore`
(`store-sqlite`), `CockroachStore` (`store-cockroach`).

### `Embedder` (`src/embed/mod.rs`)
```rust
pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}
```
Implementations: `FixtureEmbedder` (`embed-fixture`), `BgeM3LlamaCppEmbedder`
(`embed-bge`), Bedrock Titan (`embed-bedrock`, authorization-gated).

## Single-writer lease

Spec §2.2 single-writer is **store-enforced** (T8.6). A per-session lease row
carries the holder (agent + pid + host), acquired-at, and a TTL.

- `GraphStore::acquire_lease` / `refresh_lease` / `release_lease`.
- Acquire is atomic per backend (`INSERT ... ON CONFLICT ... WHERE (expired OR mine) RETURNING`).
- `Memory::build()` acquires or fails closed naming the current holder; `close()` releases; a heartbeat (interval = TTL/3) keeps a live holder and lets a crashed one expire.
- **Fence:** if the heartbeat observes the lease was taken over, the writer is latched closed — further writes and the flush are refused, and pending is dropped rather than overwrite the new holder.
- **Known bound:** the fence trips on *detection*, not at the instant of loss — an old holder may write for up to ~one heartbeat interval + one flush cycle after a legitimate TTL expiry. Tracked as [nrynss/lambo#1](https://github.com/nrynss/lambo/issues/1) (store-side fencing tokens).

TTL 45s, heartbeat 15s, and `LEASE_TTL > SHUTDOWN_BUDGET` (15s) is a compile-time
assertion so a graceful shutdown releases rather than expires.

See also: [mcp.md](mcp.md), [config.md](config.md).
