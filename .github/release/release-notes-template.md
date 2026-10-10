# Lambo v__LAMBO_VERSION__

> Maintainer: trim the checklists below to the actual release contents before
> publishing. The version is substituted automatically by the release
> workflow; do not hand-edit it here.

Lambo is agentic graph memory. This single binary carries the MCP server
(`lambo serve`) and the CLI verbs (`lambo recall`, `lambo derive`, `lambo
record_action`, ...). The API is the Rust library crate, consumed as a Cargo
dependency rather than distributed as an executable.

## What's new

- _Summarize the user-visible changes since the previous release, one bullet each._

## Features included

Every binary in this release carries the full `ship` adapter set. You pick the
store and embedder at runtime in `lambo.toml`; switching among the adapters
below never needs a different download.

- Stores: `memory`, `sqlite`, `cockroach`, `postgres` (PostgreSQL + pgvector)
- Embedders: `fixture`, `bge_m3` (also reached as `openai`), `gemini` (Vertex
  `gemini-embedding-001`), `embeddinggemma2` (EmbeddingGemma 2, text and
  images, feature `embed-eg2`)
- Recall tier: `elastic` (feature `recall-elastic`), an optional Elasticsearch
  index selected by a top-level `[recall]` section that serves recall's vector
  leg beside the store
- Not included: `bedrock` (Amazon Bedrock is gated on account authorization and
  lands in a later release)

Apple silicon also gets a Metal build, `lambo-__LAMBO_VERSION__-macos-arm64-metal`:
the same adapter set plus the in-process `candle` embedder on the GPU
(`[embedder] kind = "candle"`, `device = "metal"`). The stock binaries do not
carry candle, so the in-process GPU embedder is the one case where you choose
a different download. It links only macOS system frameworks, so it needs nothing
installed beyond macOS itself. Set `LAMBO_FLAVOR=metal` for the install script
to pick it; the script refuses that flavor on any other platform.

The one caveat: the adapter code is compiled in, but its backing service must be
reachable at runtime. BGE embeddings need a local `llama-server` or a hosted
OpenAI-compatible endpoint (`api_key_env`). CockroachDB
needs a reachable cluster. Postgres needs a reachable server with pgvector.
Gemini needs Vertex AI credentials. EmbeddingGemma 2 needs `llama-server`
b11452 or later; images need it started with `--mmproj` and the image-budget
flags in `lambo.example.toml` (without `--mmproj`, set `[embedder] images =
false`). The Elasticsearch tier needs a reachable cluster; the store stays the
source of truth, and recall's vector leg falls back to the store while the
index is stale or failing.

`cargo install lambo` is a leaner channel: it builds the crate's default
features (the `memory` store, `fixture` + `bge_m3` embedders) rather than the
full `ship` set the prebuilt binaries carry. Use the prebuilt binaries above,
or build from source with `--features ship`, for the complete adapter set.

## Binary checksums

Each platform release has a binary and a `.sha256` file, for example
`lambo-__LAMBO_VERSION__-linux-x86_64.sha256`. Verify a download from the
directory holding both files: `sha256sum -c <asset>.sha256` on Linux,
`shasum -a 256 -c <asset>.sha256` on macOS (which ships no `sha256sum`).

| Platform | Asset |
|---|---|
| Linux x86_64 | `lambo-__LAMBO_VERSION__-linux-x86_64` |
| Linux arm64 | `lambo-__LAMBO_VERSION__-linux-arm64` |
| macOS arm64 | `lambo-__LAMBO_VERSION__-macos-arm64` |
| macOS arm64, Metal (candle) | `lambo-__LAMBO_VERSION__-macos-arm64-metal` |

There is no Windows binary in this release: the shared session endpoint does not compile on Windows yet ([#39](https://github.com/nrynss/lambo/issues/39)). v0.2.2 is the last release with a Windows build.

## Install

Install the latest release with the install script:

```bash
curl -fsSL https://github.com/nrynss/lambo/releases/latest/download/install.sh | sh
```

Or pin this version. The variables go on `sh`, the command that reads them;
add `LAMBO_INSTALL_DIR=/some/dir` there too to install somewhere other than
`~/.local/bin`:

```bash
curl -fsSL https://github.com/nrynss/lambo/releases/download/v__LAMBO_VERSION__/install.sh | LAMBO_VERSION=__LAMBO_VERSION__ sh
```

The Metal build on Apple silicon, pinned:

```bash
curl -fsSL https://github.com/nrynss/lambo/releases/download/v__LAMBO_VERSION__/install.sh | LAMBO_VERSION=__LAMBO_VERSION__ LAMBO_FLAVOR=metal sh
```

## Known limits

- Bedrock embeddings are not shipped (blocked on account authorization).
- _Add any limits specific to this release._

## Build from source

Prebuilt binaries are the primary channel. To build from source instead:

```bash
git clone https://github.com/nrynss/lambo.git
cd lambo
cargo build --release --features ship
# Apple silicon, with the Metal candle embedder:
cargo build --release --features ship,embed-candle-metal
```

The full-feature build is the `ship` profile. For a leaner binary, pick the
adapters you need from the Cargo features table in the installation guide
(`docs/reference/installation.mdx`), which lists every store and embedder
feature, including `store-postgres`, `embed-gemini` and the `embed-candle`
accelerator features.

## Verify

```bash
lambo --version   # must print the release version
```
