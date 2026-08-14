# Configuration

Lambo reads a `lambo.toml` file. It looks for the file in this order: the `--config` path, then the `LAMBO_CONFIG` environment variable, then `./lambo.toml`. An environment variable always wins over the file for any key it sets.

## Store

The `[store]` section selects where the graph is persisted.

```toml
[store]
kind = "memory"
```

Set `kind` to `memory`, `sqlite`, or `cockroach`. The kind you choose must be compiled into your binary. If it is not, Lambo fails at startup with a clear message. See [Installation](installation.md) for the matching features.

## Embedder

The `[embedder]` section selects how text is turned into vectors.

```toml
[embedder]
kind = "fixture"
dim = 1024
```

Set `kind` to `fixture`, `bge`, or `bedrock`. Do not switch embedder models in the middle of a session without re-embedding, because vectors from different models are not comparable.

## Tunables

You can override the defaults for scoring, recall, flushing, and canonization. The most common keys are below. The `Config` struct in the source lists every key with its default, and `lambo.example.toml` shows a full file.

| Key | What it controls |
|---|---|
| `backend_flush_interval` | How often pending writes flush to the store. This sets how far behind durable storage a session can be. |
| `backend_flush_max_batch`, `backend_flush_retries`, `backend_log_max` | Flush batch size, retry count, and log cap. |
| `daemon_tick_interval` | How often the background workers run. |
| `default_top_k`, `default_max_tokens`, `default_traversal_depth` | The recall defaults used when a caller omits them. |
| `match_strategy` | `Canonical` or `Hybrid`. |
| `max_canonical_nodes` and the `canonization_` keys | Canonization thresholds and cadence. |
| `scoring`, `recall_weights` | The scoring and recall weight vectors. |

## HTTP transport

The HTTP transport binds to `127.0.0.1` by default. It has no authentication, no rate limiting, and no cap on the number of sessions it creates. Keep it on localhost. Do not expose it to a network until authentication is added.

## Provisioning a durable store

Before you serve against SQLite or CockroachDB, create the schema with the provisioning script.

```bash
./scripts/provision.sh
```

Set `LAMBO_COCKROACH_DSN` in a `.env` file first for CockroachDB. Run `./scripts/provision.sh --check` to verify the schema without applying it.

See [Installation](installation.md) and [Library API](api.md).
