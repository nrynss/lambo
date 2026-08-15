# Lambo

Agentic graph memory for multi-agent coding. Lambo stores what your agents learn and recalls
it by meaning. A background process promotes concepts to canonical facts when they earn it
from structural evidence, not when an agent declares them important.

**[Documentation](https://nrynss.github.io/lambo)** covers everything below in depth.

## Deployment model

One `lambo serve` process owns a session and is its **single writer**. Agents connect over
MCP, by stdio or HTTP, and share that process's in-memory graph. Mutations flush
asynchronously to the durable store.

Any number of readers query the store directly and see eventually consistent state.
Dashboards and the CockroachDB managed MCP server work this way. Readers never write.
Multi-writer coordination stays out of scope for v0.1.

## Install and run

Lambo ships as one binary carrying every adapter. You install it, then write a `lambo.toml`
that picks a store and an embedder at runtime.

```bash
curl -fsSL https://github.com/nrynss/lambo/releases/latest/download/install.sh | sh
```

The script verifies a SHA-256 checksum and installs to `~/.local/bin`. Windows binaries sit
on the [releases page](https://github.com/nrynss/lambo/releases).

```toml
# lambo.toml
[store]
kind = "memory"     # memory | sqlite | cockroach

[embedder]
kind = "fixture"    # fixture | bge_m3 | bedrock
dim = 1024
```

```bash
lambo serve --config lambo.toml --session demo --agent agent-a
```

Switching to a durable store means editing that file, not downloading a different build. See
[Quickstart](https://nrynss.github.io/lambo/quickstart/) for the 30 second version and
[Installation](https://nrynss.github.io/lambo/installation/) for MCP client setup, real
embeddings, and building from source.

## Pluggable backends

Cargo features decide which adapters get compiled in. `lambo.toml` decides which one runs.
Environment variables override the file, so a cluster DSN stays out of it. Copy
[`lambo.example.toml`](lambo.example.toml) to start, and see
[Configuration](https://nrynss.github.io/lambo/config/) for every key and override.

Released binaries carry the full adapter set, so picking a backend is a config decision
rather than a build decision. Building your own with a narrower feature list is covered in
[Installation](https://nrynss.github.io/lambo/installation/). The design of record is
[`dev-diary/notes/level-b-pluggability.md`](dev-diary/notes/level-b-pluggability.md).

**Do not switch embedder models mid-session without re-embedding.** Vectors from different
models are not comparable. Session identity is the `EmbeddingContract` of kind, model, and
dimension.

## Architecture

```mermaid
flowchart LR
    subgraph Surfaces["Agent surfaces"]
        MCP["MCP clients<br/>(agents)"]
        CLI["Command line verbs"]
        WEB["lambo serve-web<br/>(browser, read-only)"]
    end
    subgraph Writer["lambo serve (single writer)"]
        GRAPH["In-memory graph<br/>concepts and edges"]
        DAEMON["Background daemon"]
        CANON["Canonization"]
        EMB["Embedder"]
    end
    subgraph Durable["Durable store"]
        DB[("memory / sqlite /<br/>cockroach")]
    end
    MCP --> GRAPH
    CLI --> GRAPH
    WEB -. "read-only" .-> DB
    GRAPH --> EMB
    GRAPH --> DAEMON
    DAEMON --> CANON
    GRAPH -. "write-behind flush" .-> DB
```

## Reference

| Page | What it covers |
|---|---|
| [MCP tools](https://nrynss.github.io/lambo/mcp/) | The seven tools agents call, with arguments and limits |
| [Command line](https://nrynss.github.io/lambo/cli/) | The same operations as deterministic one-shot lines |
| [Configuration](https://nrynss.github.io/lambo/config/) | Every `lambo.toml` key, env override, and feature flag |
| [Library API](https://nrynss.github.io/lambo/api/) | The Rust `Memory` API and the adapter traits |
| [End to end](https://nrynss.github.io/lambo/end-to-end/) | Serving, reading, and running a swarm against one session |
| [Demo](https://nrynss.github.io/lambo/demo/) | Two agents building one REST API on shared memory |

## CockroachDB tools used

| Tool | How Lambo uses it |
|---|---|
| **Distributed vector indexing** | `concepts.embedding VECTOR(1024)` with a vector index, powering semantic concept merging. The vectors live beside the graph they describe rather than in a separate retrieval sidecar. |
| **Cloud managed MCP server** | Connected read-only to Claude Code to inspect a live session. Querying `canonization_events` shows concepts earning status, and `edges` traces provenance. |
| **ccloud CLI** | Cluster creation and DSN capture, following [`dev-diary/notes/spike-runbook.md`](dev-diary/notes/spike-runbook.md). [`scripts/provision.sh`](scripts/provision.sh) then applies the schema idempotently. |

## AWS services used

| Service | How Lambo uses it |
|---|---|
| **Amazon Bedrock** | Titan Text Embeddings V2 behind the `Embedder` trait at 1024 dimensions, through `aws-sdk-bedrockruntime` under the `embed-bedrock` feature. The adapter ships, and BGE-M3 remains the default dense path while account authorization is pending. |

## Development

Phase by phase handoffs, adversarial reviews, and evidence live in [`dev-diary/`](dev-diary/).
Run `cargo test` for the suite. The docs site source sits in [`site/`](site/) and deploys to
GitHub Pages from `main`.

## License and credit

MIT. See [LICENSE](LICENSE).

Lambo v0.1 was written during the hackathon submission period. It draws on a prior v0.6.0
design document by the same author, credited here for honesty. No code from that document
was incorporated.
