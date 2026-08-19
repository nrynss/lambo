# Dogfood rig — per-machine setup and client wiring

Companion runbook to [DOGFOOD.md](DOGFOOD.md) (the design; read it first). This file is the
replicable part: run it on any machine, Mac or Linux, and you get the same rig.

**Stores do not synchronize yet, by design.** Until workstream B lands, each machine's rig
is its own graph — same setup, separate memories. Replication here means *same setup*, not
shared state. When B lands, the `[store]` block flips to a shared Postgres and nothing
else changes.

---

## 1. The embedder (same artifact everywhere — that is the rule)

The space is defined by the model artifact including quantization, so every machine uses
the **same GGUF**, checksum-verified:

```
source:  https://huggingface.co/ggml-org/bge-m3-Q8_0-GGUF  (bge-m3-q8_0.gguf, 605 MB)
sha256:  aa473d51f451a22f0fcf39ba3330c14bed38a385712b1113440f69df4047a173
```

```sh
mkdir -p ~/models
curl -sSL -o ~/models/bge-m3-q8_0.gguf \
  "https://huggingface.co/ggml-org/bge-m3-Q8_0-GGUF/resolve/main/bge-m3-q8_0.gguf"
shasum -a 256 ~/models/bge-m3-q8_0.gguf   # Linux: sha256sum
```

llama-server install: **Mac** `brew install llama.cpp` (Metal automatic). **Linux**
package manager if it carries llama.cpp, else build from source (`cmake -B build
-DGGML_CUDA=ON` on the 4070 desktop; plain CPU build is fine for embeddings — BGE-M3 is
small).

Run (identical on both platforms):

```sh
nohup llama-server --embedding -m ~/models/bge-m3-q8_0.gguf \
  --port 8080 --host 127.0.0.1 > /tmp/llama-embed-8080.log 2>&1 &
curl -s 127.0.0.1:8080/health   # {"status":"ok"}
```

Keep it on `127.0.0.1`. (Sharing one embedder over LAN works — instances are fungible —
but binds a network service for no real saving; the model is 605 MB.)

## 2. The pinned binary (per machine, per arch — binaries do not travel)

```sh
cd <lambo checkout> && git checkout lambo-for-mooshik   # pin: see DOGFOOD.md, currently 3039b82
cargo build --release --features store-sqlite,embed-bge
mkdir -p ~/lambo-dogfood/bin
cp target/release/lambo ~/lambo-dogfood/bin/lambo-<sha>
```

The copy out of `target/` is the isolation rule: rebuilds and `cargo clean` must not be
able to touch the serving binary. Upgrading = build at a newer sha, copy, re-register,
note it in the session.

## 3. Store config

`~/lambo-dogfood/lambo.toml` (identical apart from `$HOME`):

```toml
[store]
kind = "sqlite"
path = "/home/or/Users/<you>/lambo-dogfood/lambo-dev.db"

[embedder]
kind = "bge_m3"
dim = 1024
llama_url = "http://127.0.0.1:8080"
```

```sh
~/lambo-dogfood/bin/lambo-<sha> provision --config ~/lambo-dogfood/lambo.toml
```

## 4. Client wiring

Every client registers the **same triple** — the pinned binary, `serve`, the config — and
differs only in the `--agent` id, so the ledger attributes writes per client:

| Client | agent id |
| --- | --- |
| Claude Code | `claude-orchestrator` |
| Codex CLI | `codex-agent` |
| Cursor / Cursor Agent CLI | `cursor-agent` |
| OMP | `omp-agent` |
| Pi | `pi-agent` |
| Grok Build | `grok-agent` |

The server command, everywhere:

```
~/lambo-dogfood/bin/lambo-<sha> serve --config ~/lambo-dogfood/lambo.toml --session lambo-dev --agent <agent-id>
```

**Claude Code** — user scope, never project scope (a project `.mcp.json` lands in this
public repo):

```sh
claude mcp add --scope user lambo-dogfood -- ~/lambo-dogfood/bin/lambo-<sha> serve \
  --config ~/lambo-dogfood/lambo.toml --session lambo-dev --agent claude-orchestrator
```

**Codex CLI** — `codex mcp add lambo-dogfood -- <command…>` with `--agent codex-agent`,
or declaratively in `~/.codex/config.toml`:

```toml
[mcp_servers.lambo-dogfood]
command = "/absolute/path/to/lambo-dogfood/bin/lambo-<sha>"
args = ["serve", "--config", "/abs/path/lambo.toml", "--session", "lambo-dev", "--agent", "codex-agent"]
```

**Cursor** — merge into `~/.cursor/mcp.json` (global, not the project file):

```json
{ "mcpServers": { "lambo-dogfood": {
    "command": "/abs/path/lambo-dogfood/bin/lambo-<sha>",
    "args": ["serve", "--config", "/abs/path/lambo.toml",
             "--session", "lambo-dev", "--agent", "cursor-agent"] } } }
```

The Cursor Agent CLI reads the same file; it already drove lambo's seven tools once
(`evidence/mcp-client-interop/`).

**OMP** — reads the **workspace `.mcp.json`** (same JSON shape as Cursor's block). Two
gotchas, both documented in `evidence/swarm/probes/`: OMP **always loads every
globally-configured MCP server and offers no flag to drop them**, and an inherited server
of the same name shadows the workspace one — the C5 probes' calls landed in an inherited
live lambo instead of the scratch store that way. So: register `lambo-dogfood` globally
OR per-workspace, never both names colliding, and prefer `--no-tools` runs when the
toolset must stay narrow (small models drift into inherited tools otherwise).

**Pi** — `pi-mcp-adapter`, reads `.mcp.json`. With small local models use
`"settings": {"toolPrefix": "none"}` plus a `-t <lambo tool names>,mcp` allowlist so only
lambo's tools are visible (established in the LFM2 rig work).

**Grok Build** — same registration shape:

```sh
grok mcp add lambo-dogfood -- ~/lambo-dogfood/bin/lambo-<sha> serve \
  --config ~/lambo-dogfood/lambo.toml --session lambo-dev --agent grok-agent
```

or declaratively in `~/.grok/config.toml`; tools appear namespaced
`lambo-dogfood__lambo_*`, stderr lands in `~/.grok/logs/mcp/lambo-dogfood.stderr.log`,
and `grok mcp doctor lambo-dogfood` diagnoses a server that starts but fails to connect.

## 5. The one-writer reality (per machine)

Each stdio registration spawns its **own** serve against that machine's SQLite file. The
write lease makes overlap safe, not simultaneous: the newest serve holds the lease and
everyone else's writes are refused with `StaleWrite` (loud, never corruption) until they
re-acquire. Switching clients is a lease handoff — restart the client you're moving to.
If genuinely simultaneous clients are wanted, that is DOGFOOD.md wiring option (c): one
`serve` over HTTP MCP, every client pointed at the URL, serialization inside the single
writer where it belongs.

## 6. Smoke test (any client)

Ask the agent to call `lambo_stats`, then `lambo_recall` with query "width authority" —
a seeded graph answers with the F property statement and the pin semantics. An empty
graph on a fresh machine is correct too (stores do not sync yet); seed it with the same
protocol: derive the current workstream decisions, record-action the standup.
