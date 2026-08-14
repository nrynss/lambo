# Installation

Lambo builds from source with Cargo. You get a single `lambo` binary that runs the server and the command line tools.

## Prerequisites

You need a stable Rust toolchain. Install it from [rustup.rs](https://rustup.rs) if you do not have it.

The default build has no other requirements, because it uses an in-memory store and a built-in fixture embedder. For real use you also want:

- A durable store. Use SQLite for a local file, or CockroachDB for a hosted cluster.
- A real embedder. Lambo uses BGE-M3 served by a local `llama-server`.

## Build

Clone the repository and build the release binary.

```bash
git clone https://github.com/nrynss/lambo.git
cd lambo
cargo build --release --features demo
```

The binary is written to `target/release/lambo`. Put it on your `PATH` or call it by that path.

Confirm the build works.

```bash
lambo --help
```

## Choose your features

Cargo features select which stores and embedders are compiled in. The `demo` profile used above includes all of them. To build a smaller binary, list only what you need.

| Feature | What it adds |
|---|---|
| `store-memory` | In-memory store. This is the default and needs no external service. |
| `store-sqlite` | SQLite store, backed by a local file. |
| `store-cockroach` | CockroachDB store, for a hosted cluster. |
| `embed-fixture` | Deterministic fixture embedder for tests and quick starts. |
| `embed-bge` | BGE-M3 embeddings served by `llama-server`. |

For example, to build with SQLite and BGE embeddings only:

```bash
cargo build --release --features store-sqlite,embed-bge
```

## Run it the first time

The fastest way to see Lambo work uses the in-memory store and the fixture embedder, so it needs no external services. Create a `lambo.toml`.

```toml
[store]
kind = "memory"

[embedder]
kind = "fixture"
dim = 1024
```

Start a session server.

```bash
lambo serve --config lambo.toml --session demo --agent agent-a
```

Your agent can now connect over MCP. See [MCP tools](mcp.md) for the tool surface, and [End to end](end-to-end.md) for a full walkthrough.

## Set up real embeddings

To use BGE-M3 embeddings, run a local `llama-server` with an embedding model and point Lambo at it.

```bash
llama-server -m path/to/bge-m3.gguf --embedding --port 8080
```

Then set the embedder in `lambo.toml` or through environment variables.

```toml
[embedder]
kind = "bge"
dim = 1024
```

Do not switch embedder models in the middle of a session without re-embedding, because vectors from different models are not comparable.

## Set up durable storage

For a durable store, run `scripts/provision.sh` to create the schema before you serve. See [Configuration](config.md) for the store keys and the provisioning details.
