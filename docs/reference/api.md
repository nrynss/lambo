# Library API

Both surfaces, the MCP tools and the command line verbs, are thin wrappers over one type, `Memory`. If you embed Lambo in a Rust program, this is the type you work with. For full signatures, run `cargo doc --open`.

## Memory

`Memory` holds the in-memory graph together with its background workers, which handle write-behind flushing and canonization. Exactly one `Memory` writes a given session. See [The single-writer lease](#the-single-writer-lease).

### Build a Memory

Use the builder. Prefer `backends`, which supplies the store, embedder, and their compatibility in one value, because it is the single place backends are constructed.

```rust
let mem = Memory::builder()
    .session("my-session")
    .agent("agent-a")
    .backends(resolved)
    .build()
    .await?;
```

The builder accepts `session`, `agent`, `store`, `embedder`, `embedding_contract`, `backends`, `match_strategy`, `flush_interval`, `scoring_weights`, and `config`.

`build().await` acquires the writer lease, fails if another process holds it, runs the startup load, and starts the background workers.

### Read the session

The accessors `session`, `agent`, `config`, `embedding_contract`, `graph`, `index`, and `store` return the current state.

### Write

| Method | What it does |
|---|---|
| `derive` | Derives concepts from an interaction. |
| `record_action` | Records an action as a concept plus its causal and dependency edges. |
| `demote` | Demotes a chunk. |
| `retract` | Retracts a concept. Pass a dry-run flag to get an impact report without mutating. |
| `reserve` and `release` | Take or release an advisory soft lock. |
| `set_root_goal` and `declare_synonym` | Set up the session. |

### Read

| Method | What it returns |
|---|---|
| `recall` | The context block for a query. |
| `canonical_memories` | The session's canonical memories. |
| `stats` | Flush lag, log depth, counts, and degraded state. |
| `events` | A live feed of background events. |

### Shut down

`close().await` flushes the pending write-behind tail and releases the lease. It is time-bounded. If a dependency hangs, it stops rather than waiting forever, and it reports honestly that the un-flushed tail was not saved. There is no on-disk write-ahead log, so an abandoned tail is lost, not replayed on restart.

## Backends

Backends are chosen by Cargo feature and configuration, not loaded as plugins. See [Configuration](config.md) for the feature flags.

### The GraphStore trait

`GraphStore` handles durable graph storage. The implementations are the in-memory store, the SQLite store, and the CockroachDB store. Its methods cover schema setup, flushing a batch, loading a session, keyword and vector candidate lookup, blast radius, and canonization records, plus the lease methods below.

### The Embedder trait

```rust
pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}
```

The implementations are the fixture embedder, BGE-M3 over `llama-server`, and Amazon Titan through Bedrock.

## The single-writer lease

A session has exactly one writer. Lambo enforces this in the store with a per-session lease that records the current holder and a time-to-live.

- `build()` acquires the lease or fails, naming the process that holds it. `close()` releases it.
- A heartbeat keeps a live holder's lease fresh and lets a crashed holder's lease expire.
- If a holder's lease is taken over, that holder is fenced. Its further writes are refused and its pending writes are dropped, so it cannot overwrite the new holder.

There is one known limitation. The fence trips when the holder's heartbeat notices the loss, not at the instant it happens. After a lease expires and a new writer takes over, the old holder can still write for a short, bounded window. This matters most when many writers contend on one session. It is tracked in [issue #1](https://github.com/nrynss/lambo/issues/1).

See [MCP tools](mcp.md) and [Configuration](config.md).
