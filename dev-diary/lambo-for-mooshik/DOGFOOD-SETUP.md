# Dogfood rig — per-machine setup and client wiring

Companion runbook to [DOGFOOD.md](DOGFOOD.md) (the design; read it first). This file is the
replicable part: run it on any machine, Mac or Linux, and you get the same rig.

**Stores do not synchronize yet, by design.** Until workstream B lands, each machine's rig
is its own graph — same setup, separate memories. Replication here means *same setup*, not
shared state. When B lands, the `[store]` block flips to a shared Postgres and nothing
else changes.

---

## 1. The embedder (same artifact everywhere — that is the rule)

> **Since 2026-08-23 this section is OPTIONAL for running the rig.** The dogfood rig
> embeds in-process via candle (§3), so no llama-server is required. Keep this section
> for two live uses: reproducing K1's parity captures, which measure candle *against*
> this llama.cpp reference, and the `kind = "bge_m3"` fallback config. The same-artifact
> rule is unchanged and now spans both paths — the GGUF here and the f16 safetensors
> candle loads are both casts of the same canonical `BAAI/bge-m3` revision, which is why
> they agree to a median cosine of 0.9998.

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
  --port 8080 --host 127.0.0.1 \
  --batch-size 8192 --ubatch-size 8192 -c 8192 \
  > /tmp/llama-embed-8080.log 2>&1 &
curl -s 127.0.0.1:8080/health   # {"status":"ok"}
```

**The three capacity flags are load-bearing, not tuning** (found by K1's NVIDIA leg
2026-08-22, confirmed live on the Mac rig the same day). Without them this build clamps
`n_batch = n_ubatch = 512`, and **any input over ~512 tokens fails with HTTP 500** —
`input (N tokens) is too large to process. increase the physical batch size`. That is the
J3-R3-1 failure class reachable by plain input length: before J3 the write applied with
`embedding = NULL` while the receipt said "applied", so on a pre-J3 binary every
long-content write silently lost its vector. BGE-M3 carries 8192-token capacity; the
defaults throw away fifteen sixteenths of it. Long flags only — this build rejects `-u`.
Verify after starting, not just `/health`:

```sh
python3 - <<'EOF'
import json, urllib.request
req = urllib.request.Request("http://127.0.0.1:8080/v1/embeddings",
    data=json.dumps({"input": "word " * 900, "model": "bge-m3"}).encode(),
    headers={"Content-Type": "application/json"})
print(len(json.load(urllib.request.urlopen(req, timeout=60))["data"][0]["embedding"]))
EOF
# 1024 = capacity is real; HTTP 500 = the flags did not take
```

Keep it on `127.0.0.1`. (Sharing one embedder over LAN works — instances are fungible —
but binds a network service for no real saving; the model is 605 MB.)

## 2. The pinned binary (per machine, per arch — binaries do not travel)

```sh
cd <lambo checkout> && git checkout lambo-for-mooshik   # pin: see DOGFOOD.md, currently bbef4b3
LAMBO_GIT_SHA=$(git rev-parse --short HEAD) \
  cargo build --release --features store-sqlite,embed-candle-metal,embed-bge
# NVIDIA/Linux rig (this machine): --features store-sqlite,embed-candle-cuda,embed-bge;
# metal is Apple-silicon-only. CUDA must be on the build PATH (the unit's Environment
# already exports /opt/cuda/bin); on an Apple box keep embed-candle-metal.
mkdir -p ~/lambo-dogfood/bin
cp target/release/lambo ~/lambo-dogfood/bin/lambo-<sha>
```

`LAMBO_GIT_SHA` is **not optional and not cosmetic.** It is an `option_env!` read at
compile time (`src/ledger.rs`), and it is the only thing that makes the ledger's `stats`
heartbeat able to say *which* pinned binary produced a stretch of ledger. Omit it and the
field is `"unknown"` — so two builds at different commits are indistinguishable in the
file, and the I2 property "an upgrade shows as a sha change" is unobtainable however
carefully the upgrade is performed. This build step is the one place in the rig that can
set it; nothing downstream can recover it.

Build with a dirty tree and the sha is still the last commit's, which is a lie about the
binary. Commit (or stash) first, or the heartbeat attributes the run to code that is not
in it.

**A store provisioned by an older lambo needs `lambo provision` before the new binary
can write to it** (idempotent; found doing the 2026-08-23 K2 migration, where the store
was missing `lease_refusals` and `write_intents`). The re-embed refused with exactly that
diagnosis rather than half-migrating, which is the behaviour to expect:

```sh
~/lambo-dogfood/bin/lambo-<sha> provision --config ~/lambo-dogfood/lambo.toml
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
kind = "candle"
dim = 1024
device = "metal"        # Apple silicon; use "cuda" on an NVIDIA box
```

Since the 2026-08-23 K2 migration the rig embeds **in process** — no llama-server, no
`llama_url`. `device` is pinned rather than left `auto` so a missing accelerator fails
loudly instead of resolving to a CPU path measured at ~3% of the llama-server baseline.
Weights are the published f16 safetensors (`nrynss/bge-m3-f16-safetensors`), fetched once
(~1.1 GB) into the HF cache and hash-checked at load.

The pre-migration block is kept verbatim for the llama-server path, which still works and
is what §1 serves:

```toml
[embedder]
kind = "bge_m3"
dim = 1024
llama_url = "http://127.0.0.1:8080"
```

```sh
~/lambo-dogfood/bin/lambo-<sha> provision --config ~/lambo-dogfood/lambo.toml
```

## 4. Client wiring

Every client points at the **same URL** and holds no lease. Per-client attribution does not
come from the transport: `origin_agent` is taken from each call's own `agent_id`, above it,
so one shared writer still attributes every write to the harness that made it (verified
2026-08-23). The `--agent` on the writer names only the **lease holder**, hence
`http-shared-writer`.

| Client | `agent_id` it sends |
| --- | --- |
| Claude Code | `claude-orchestrator` |
| Codex CLI | `codex-agent` |
| Cursor / Cursor Agent CLI | `cursor-agent` |
| OMP | `omp-agent` |
| Pi | `pi-agent` |
| Grok Build | `grok-agent` |

**The `--ledger` flags now live on the supervised unit (§5), not on any registration** —
which is most of the point. They are off by default, so under the old per-client wiring a
single block that forgot them yielded a serving rig that measured nothing for that
harness, and the omission was invisible (Cursor's registration had exactly this defect
until 2026-08-23). DOGFOOD metrics 1, 2, 4 and 5 are all computed from the ledger by
[`scripts/observability/`](../../scripts/observability/README.md), and there is no
after-the-fact way to reconstruct a call that was never recorded. `--ledger-heartbeat 300`
is what makes the counts quotable — without it the file carries no `ledger_dropped_lines`,
so every report has to say `dropped: UNKNOWN` and no number in it can be trusted as
complete. One unit now carries both flags for every client at once.

The path is deliberately **outside the repo** (I1 hygiene: the ledger carries recall
queries and truncated concept text, and reaches `evidence/` only through the curated
export path). Every client's calls land in that one file, which is intended: one file,
`agent_id` per line, so per-client attribution is a `GROUP BY` rather than a set of files
to reconcile. Rotation is the operator's (`logrotate`, or just `mv` it — the writer
reopens the path per batch).

**Since the 2026-08-23 HTTP ruling (§5) every block below is a URL, not a command.** No
harness spawns a `serve`, so none of them owns the writer's lifetime, none takes the lease,
and the `--ledger` flags live on the supervised unit rather than on six registrations. The
stdio forms are kept at the end of this section as history — they are what the ruling
replaced, and reading them explains the shape of the URL blocks.

**Claude Code** — user scope, never project scope (a project `.mcp.json` lands in this
public repo). In `~/.claude.json` under `mcpServers`:

```json
{ "lambo-dogfood": { "type": "http", "url": "http://127.0.0.1:7700/mcp" } }
```

**Codex CLI** — `~/.codex/config.toml`, the same `url` form its `endor-docs` entry uses:

```toml
[mcp_servers.lambo-dogfood]
url = "http://127.0.0.1:7700/mcp"
```

**Cursor** — `~/.cursor/mcp.json` (global, not the project file). The Cursor Agent CLI
reads the same file; it already drove lambo's seven tools once
(`evidence/mcp-client-interop/`):

```json
{ "mcpServers": { "lambo-dogfood": { "url": "http://127.0.0.1:7700/mcp" } } }
```

All three were migrated on the MacBook on 2026-08-23 and the lease holder was confirmed
`http-shared-writer` afterwards. Two things that migration found, worth expecting:

* **Cursor was pinned to `lambo-3039b82`** — two generations stale. It predated the K2
  migration, so it was built without candle and would have failed outright against the
  current `kind = "candle"` config; it also carried **no `--ledger` flags**, so anything
  written through Cursor was never measured. A shared writer removes this whole class:
  there is one binary and one ledger configuration to keep current instead of six.
* **Codex had no lambo entry at all.** Per-client stdio registration is easy to forget and
  invisible when forgotten — the harness simply has no memory and says nothing.

<details>
<summary>The pre-2026-08-23 stdio registrations (history — do not use)</summary>

```sh
# Claude Code
claude mcp add --scope user lambo-dogfood -- ~/lambo-dogfood/bin/lambo-<sha> serve \
  --config ~/lambo-dogfood/lambo.toml --session lambo-dev --agent claude-orchestrator \
  --ledger ~/lambo-dogfood/calls.jsonl --ledger-heartbeat 300
```

```toml
# Codex CLI — ~/.codex/config.toml
[mcp_servers.lambo-dogfood]
command = "/absolute/path/to/lambo-dogfood/bin/lambo-<sha>"
args = ["serve", "--config", "/abs/path/lambo.toml", "--session", "lambo-dev", "--agent", "codex-agent",
        "--ledger", "/abs/path/lambo-dogfood/calls.jsonl", "--ledger-heartbeat", "300"]
```

```json
{ "mcpServers": { "lambo-dogfood": {
    "command": "/abs/path/lambo-dogfood/bin/lambo-<sha>",
    "args": ["serve", "--config", "/abs/path/lambo.toml",
             "--session", "lambo-dev", "--agent", "cursor-agent",
             "--ledger", "/abs/path/lambo-dogfood/calls.jsonl",
             "--ledger-heartbeat", "300"] } } }
```

</details>

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
  --config ~/lambo-dogfood/lambo.toml --session lambo-dev --agent grok-agent \
  --ledger ~/lambo-dogfood/calls.jsonl --ledger-heartbeat 300
```

or declaratively in `~/.grok/config.toml`; tools appear namespaced
`lambo-dogfood__lambo_*`, stderr lands in `~/.grok/logs/mcp/lambo-dogfood.stderr.log`,
and `grok mcp doctor lambo-dogfood` diagnoses a server that starts but fails to connect.

## 4b. The protocol reaches agents through instructions, not tools

Registration gives an agent the tools; nothing about MCP makes it *use* them. Three
layers, by client capability:

- **AGENTS.md readers** (Codex, Cursor, Claude Code): the repo's `AGENTS.md`
  §"Consulting memory during development work" is the always-on protocol — recall before
  a workstream, derive decisions-with-why, record-action merges, warnings block. Nothing
  further to configure.
- **Orchestrated subagents**: the orchestrator recalls and injects the hits into the
  brief verbatim (stronger than a skill — deterministic), and requires derived decisions
  in the report.
- **Small local models / OMP**: instructions must be the *system prompt* — the C5
  evidence is unambiguous that tools alone produce flailing while the skill text produces
  43/43 recall-first. Reuse the `skills/lambo-cloudops/SKILL.md` pattern: hand the
  protocol text directly to the harness (`omp` system prompt, Pi's skill slot).

## 5. The one-writer reality (per machine) — corrected by the first live session

Each stdio registration spawns its **own** serve against that machine's SQLite file, and
**what actually happens to the losers is worse than fencing: they exit 1 at startup, and
the client may surface no error to the agent at all** — a silent memory outage, observed
2026-08-19 with Claude Code + pi and now workstream
[J — Multi-client survivability](J-multi-client.md). The lease itself is correct and
stays; the wiring below is the interim rule until J2 (a losing serve becomes a proxy to
the holder) lands:

- **One client registered at a time**, or
- **More than one client on the machine ⇒ HTTP transport** (J5's stated default): one
  `serve` process, every client pointed at the URL. And a transport migration touches
  **every config layer on the machine** — a stale user-scope `command` entry beside a new
  `url` produced a client that rejected the server outright (J5's second finding).

After J2, the per-client stdio wiring in §4 simply works — the first serve becomes the
hub, later ones proxy — with no client config change.

**That is no longer the rig's wiring. Operator ruling, 2026-08-23: HTTP, always.**
J2 fixed the *silent* half of the stdio problem, not the *lifetime* half. Under stdio the
lease holder is a child of whichever client spawned it first, and there is still no
in-process promotion (J-multi-client.md, "Not done, deliberately"), so when that client
exits every other session on the machine loses memory until a human starts a new one.
Measured live the same day: holder died 06:47:24Z, **no holder for 6m20s**, recovery only
because a person opened a new session.

The rig therefore runs one long-lived writer owned by **the machine's supervisor**, not by
any client — systemd on Linux, launchd on macOS. The supervisor is the point: no client can
own the writer's lifetime, which is the whole defect being fixed. HTTP alone would not do
it; a supervised process is what makes a client's exit survivable.

```sh
# Linux
systemctl --user status lambo-dogfood      # unit: ~/.config/systemd/user/lambo-dogfood.service
# macOS
launchctl print gui/$(id -u)/dev.lambo.dogfood   # ~/Library/LaunchAgents/dev.lambo.dogfood.plist

# either platform — the check that matters
sqlite3 ~/lambo-dogfood/lambo-dev.db "select holder from session_leases;"
```

Every harness points at `http://127.0.0.1:7700/mcp` and holds no lease. Two properties the
unit depends on, both verified 2026-08-23 by SIGKILLing the writer:

- **The restart interval must exceed `lease::LEASE_TTL` (45s).** An abrupt death does not
  release the lease, so any retry inside the TTL exits 1 on a refused acquire. The knob and
  the hazard differ by platform:
  - **systemd:** `RestartSec` > 45s, and `StartLimitIntervalSec` **0**. With the default
    limit (5 starts / 10s) the unit burns its budget in the first ten seconds and is left
    FAILED permanently — the very never-recovers outage this ruling exists to prevent.
    Measured unattended recovery with the fix: **36s**.
  - **launchd:** `ThrottleInterval` **60** (default is 10s, squarely inside the TTL) with
    `KeepAlive`. There is deliberately **no `StartLimitIntervalSec` analogue and none is
    needed**: launchd has no permanent-FAILED state, so the catastrophic half of the
    systemd hazard cannot occur — it retries indefinitely. Measured unattended recovery on
    the MacBook, 2026-08-23, by `kill -9` on the writer: **64s** (60s throttle, then ~4s to
    acquire the lapsed lease). Slower than systemd's 36s because `ThrottleInterval` is a
    floor rather than a target; that is the right trade against an outage with no bound.
- The HTTP holder still binds the unix session endpoint, so a stray stdio registration
  degrades to a J2 proxy rather than breaking. That is what makes a partial migration safe,
  but a stdio client that starts while the writer is restarting can still WIN the lease and
  re-couple the holder to a client — so `select holder from session_leases` naming anything
  other than `http-shared-writer` means a harness is still on stdio and should be moved.

### The macOS unit, in full

`~/Library/LaunchAgents/dev.lambo.dogfood.plist`, then
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.lambo.dogfood.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.lambo.dogfood</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/&lt;you&gt;/lambo-dogfood/bin/lambo-&lt;sha&gt;</string>
    <string>serve</string>
    <string>--config</string><string>/Users/&lt;you&gt;/lambo-dogfood/lambo.toml</string>
    <string>--session</string><string>lambo-dev</string>
    <string>--agent</string><string>http-shared-writer</string>
    <string>--transport</string><string>http</string>
    <string>--port</string><string>7700</string>
    <string>--ledger</string><string>/Users/&lt;you&gt;/lambo-dogfood/calls.jsonl</string>
    <string>--ledger-heartbeat</string><string>300</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>60</integer>
  <key>StandardOutPath</key><string>/Users/&lt;you&gt;/lambo-dogfood/serve.log</string>
  <key>StandardErrorPath</key><string>/Users/&lt;you&gt;/lambo-dogfood/serve.log</string>
  <key>WorkingDirectory</key><string>/Users/&lt;you&gt;/lambo-dogfood</string>
</dict>
</plist>
```

`plutil -lint` the file before loading it — launchd reports a malformed plist as a
generic load failure, which is a poor way to spend ten minutes. Loopback binding means no
`--auth-token` is required; binding anywhere else makes it mandatory and `serve` refuses
to start without it, because this process is a session **writer**.

Per-harness `--agent` ids are not lost to the shared writer: `origin_agent` comes from each
call's `agent_id`, above the transport (verified 2026-08-23). The serve's own `--agent` now
names only the lease holder, hence `http-shared-writer`.

## 6. Smoke test (any client)

Ask the agent to call `lambo_stats`, then `lambo_recall` with query "width authority" —
a seeded graph answers with the F property statement and the pin semantics. An empty
graph on a fresh machine is correct too (stores do not sync yet); seed it with the same
protocol: derive the current workstream decisions, record-action the standup.

Then check the ledger actually caught it, because both of the things §2 and §4 exist to
wire are silently absent when they are wrong:

```sh
tail -2 ~/lambo-dogfood/calls.jsonl
jq -r 'select(.kind=="stats") | [.ts, .version, .git_sha] | @tsv' ~/lambo-dogfood/calls.jsonl | tail -1
```

Two lines for the two calls, and a `git_sha` that is **not** `unknown`. A `git_sha` of
`unknown` means §2's `LAMBO_GIT_SHA` did not reach the build; an absent or empty file
means §4's `--ledger` did not reach the registration. Either way the rig serves memory
fine and measures nothing, which is the failure worth catching here rather than a week
later.
