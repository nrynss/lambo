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

This release ships with the full adapter feature set compiled into one binary.
You pick the store and embedder at runtime in `lambo.toml`, not by downloading a
different binary.

- Stores: `memory`, `sqlite`, `cockroach`
- Embedders: `fixture`, `bge_m3`
- Not included: `bedrock` (Amazon Bedrock is gated on account authorization and
  lands in a later release)

The one caveat: the adapter code is compiled in, but its backing service must be
reachable at runtime. BGE embeddings need a local `llama-server`. CockroachDB
needs a reachable cluster.

`cargo install lambo` is a leaner channel: it builds the crate's default
features (the `memory` store, `fixture` + `bge_m3` embedders) rather than the
full `ship` set the prebuilt binaries carry. Use the prebuilt binaries above,
or build from source with `--features ship`, for the complete adapter set.

## Binary checksums

Each platform release has a binary and a `.sha256` file, for example
`lambo-__LAMBO_VERSION__-linux-x86_64.sha256`. Use `sha256sum` to verify a download.

| Platform | Asset |
|---|---|
| Linux x86_64 | `lambo-__LAMBO_VERSION__-linux-x86_64` |
| Linux arm64 | `lambo-__LAMBO_VERSION__-linux-arm64` |
| macOS arm64 | `lambo-__LAMBO_VERSION__-macos-arm64` |
| Windows x86_64 | `lambo-__LAMBO_VERSION__-windows-x86_64.exe` |

## Install

Install the latest release with the install script:

```bash
curl -fsSL https://github.com/nrynss/lambo/releases/latest/download/install.sh | sh
```

Or pin a version and add the install directory:

```bash
LAMBO_VERSION=__LAMBO_VERSION__ curl -fsSL https://github.com/nrynss/lambo/releases/download/v__LAMBO_VERSION__/install.sh | sh
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
```

The full-feature build is the `ship` profile. For a leaner binary, pick the
adapters you need from the table in `docs/reference/installation.mdx`.

## Verify

```bash
lambo --version   # must print the release version
```
