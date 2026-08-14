# Live-CockroachDB review runbook — T8.2 (MCP) + T8.3 (CLI)

A step-by-step review you run on a machine with real CockroachDB access. It exercises the
MCP server and the CLI against a live cluster, with real BGE-M3 embeddings over llama.cpp,
and a model-driven end-to-end via a Pi agent running LFM2.5-230M. It is written to poke the
angles the default `cargo test` cannot reach because they need a real cluster and a real
model on the wire.

Fill in the **Result** boxes as you go. The verdict template is at the end.

---

## 0. What this run is for

The store-layer, lease, and boundary findings from the T8.2/T8.3 reviews were closed against
the in-memory and SQLite stores. Three things were explicitly deferred to a live cluster:

- **N2** — control characters must never reach a Cockroach `STRING` column, because the
  error path classifies as `Other` → `is_retryable()==true` → the flush loop *retains* and
  retries forever instead of dead-lettering (queue poisoning). The MCP/CLI layer now rejects
  control chars; this run confirms that holds end to end against a real cluster.
- **The fencing-token window** ([issue #1](https://github.com/nrynss/lambo/issues/1)) — the
  single-writer lease was proven cross-process on SQLite; confirm the same on Cockroach, and
  characterise the detection-latency window under real contention.
- **Vector-index / vector-column conformance** — Cockroach persists vectors in a
  `VECTOR(n)` column; the width-agreement and index proofs only run live.

Plus the full automated suite under `--features store-cockroach`, and a real model driving
the surface so the "model-driven tool call" leg runs against durable storage, not fixtures.

---

## 1. Prerequisites and environment

You need:

- A stable Rust toolchain.
- A reachable CockroachDB cluster (Cloud or self-hosted) and its DSN.
- `llama.cpp` (`llama-server`) for two models:
  - **BGE-M3** as Lambo's embedder.
  - **LFM2.5-230M** as the Pi agent's driver model.
- `hf` (Hugging Face CLI) to fetch the models, and `pi` for the agent leg.

Set the environment. Never commit the DSN.

```bash
export LAMBO_COCKROACH_DSN='postgresql://<user>:<pass>@<host>:26257/<db>?sslmode=verify-full'
export LAMBO_STORE=cockroach
export LAMBO_EMBEDDER=bge_m3
export LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080
export LAMBO_EMBED_DIM=1024
```

Config file for the CLI and server runs.

```toml lambo.cockroach.toml
[store]
kind = "cockroach"
# dsn read from LAMBO_COCKROACH_DSN

[embedder]
kind = "bge"
dim = 1024
```

> **Result 1: environment set, DSN reachable (psql or cockroach sql connects), Rust builds**

---

## 2. Provision the schema

```bash
./scripts/provision.sh --check     # verify what would be applied
./scripts/provision.sh             # apply the schema (includes session_leases from T8.6)
```

Confirm the tables exist, including `session_leases`.

```bash
cockroach sql --url "$LAMBO_COCKROACH_DSN" -e "SHOW TABLES;"
```

> **Result 2: schema applied; session_leases present; no error**

---

## 3. Full automated suite against Cockroach

Build with the cluster and BGE features, run the default suite, then the live-gated tests.

```bash
# Compile-clean under the live feature set.
cargo fmt --all -- --check
cargo clippy --all-targets --features store-cockroach,embed-bge -- -D warnings

# Default suite (no cluster needed) — baseline must be green.
cargo test --features store-cockroach,embed-bge

# Live-cluster tests: everything #[ignore]d that needs LAMBO_COCKROACH_DSN.
cargo test --features store-cockroach,embed-bge -- --ignored
```

The live set includes: the Cockroach adapter conformance tests (`src/store/cockroach.rs`),
the canonization parity-against-live test (`src/canon/eval.rs`), the CLI saints+stats live
test (`src/cli/saints.rs`), and the live-calibration tests (`tests/live_calibration.rs`).

Then the vector-index proof (separate camera-proof gate).

```bash
LAMBO_REQUIRE_VECTOR_INDEX=1 cargo test --features store-cockroach,embed-bge -- --ignored vector
```

> **Result 3: clippy clean; default suite green (record counts); live --ignored all pass; vector-index proof passes. Record any test that skips vs fails.**

---

## 4. Store-layer and lease conformance (live)

### 4a. Single-writer lease across two processes on Cockroach

Start one server, then try to start a second on the same session. The second must be refused
by name, not hang or double-write.

```bash
# terminal A
lambo serve --config lambo.cockroach.toml --session live-lease --agent agent-a --transport stdio
# terminal B (while A holds it)
lambo serve --config lambo.cockroach.toml --session live-lease --agent agent-b --transport stdio
```

Expected in B: a fail-closed error naming `agent-a` as the holder and printing the operator
override (`DELETE FROM session_leases ...`). Confirm exactly one lease row.

```bash
cockroach sql --url "$LAMBO_COCKROACH_DSN" -e "SELECT session_id, holder, expires_at FROM session_leases;"
```

> **Result 4a: B refused, names A, one lease row, no double-writer**

### 4b. Lease release on clean shutdown and expiry on crash

- Stop A with Ctrl-C. Confirm the lease row is gone (released), and B can now acquire.
- Start A again, `kill -9` it. Confirm the row lingers until TTL (~45s), then B acquires.

> **Result 4b: clean stop releases; kill -9 expires after TTL; new holder replays via startup load**

### 4c. Fencing-token window (issue #1)

Force a takeover: let A's lease expire (kill -9, wait past TTL), acquire with B, then observe
whether A (if somehow still alive) is fenced. This characterises the bounded detection-latency
window. Note the observed window against the documented bound (~heartbeat interval + one flush
cycle).

> **Result 4c: takeover observed; old holder fenced on detection; window within documented bound**

---

## 5. MCP over Cockroach (durability + adversarial)

Start a server backed by the cluster, drive it over the wire (MCP Inspector, a client, or raw
JSON-RPC), and verify durability lands in Cockroach.

```bash
lambo serve --config lambo.cockroach.toml --session live-mcp --agent agent-a --transport stdio
```

### 5a. Happy path lands durably

Drive `lambo_derive` (a few concepts), `lambo_record_action`, `lambo_recall`, `lambo_saints`,
`lambo_stats`. Then read the store directly and confirm the rows are present.

```bash
cockroach sql --url "$LAMBO_COCKROACH_DSN" -e "SELECT session_id, content FROM concepts WHERE session_id='live-mcp' LIMIT 20;"
```

> **Result 5a: concepts and edges present in Cockroach; recall returns them; stats show flush_lag draining**

### 5b. N2 — control characters must not reach Cockroach

Send `lambo_derive` with content containing a NUL byte and a right-to-left override (U+202E). Expected: refused at the MCP layer with an honest message, `isError:true`, and
**nothing lands** in the store. Confirm the flush queue is not stuck.

```bash
cockroach sql --url "$LAMBO_COCKROACH_DSN" -e "SELECT count(*) FROM concepts WHERE content LIKE '%' || chr(0) || '%';"   # expect 0
```

Then check `lambo_stats` — `flush_lag` must drain, `dead_lettered` and `degraded` must not
climb. If a control char ever slipped through, the flush loop would retain-and-retry forever
(the poisoning N2 describes). Confirm it does not.

> **Result 5b: control chars refused; zero rows with control chars; flush queue healthy, not degraded**

### 5c. N1 — record_action fan-out cap under the real flush path

Send `lambo_record_action` with 65 combined `produces+modifies+depends_on` (over the cap of
64): expect an honest refusal. Then send several at-cap (64) calls concurrently and confirm
the process stays responsive and a `SIGTERM` mid-burst still flushes the tail to Cockroach
(`session closed, tail durable`), rather than starving the runtime.

> **Result 5c: 65 refused; concurrent at-cap calls do not starve; SIGTERM flushes durably to Cockroach**

### 5d. N3/N4 — no internal detail crosses the wire

Point the embedder at an unreachable internal URL, drive a `lambo_recall`, and confirm the
model-facing warning/error carries **no** host, port, path, or DSN. With a SQL store, confirm
a forced store error returns a class, not driver text.

> **Result 5d: warnings/errors carry a class only; no URL/DSN/host/driver text reaches the client**

### 5e. Shutdown durability against Cockroach

Repeat the T8.2 durability shape live: hold an open session, send `SIGINT` and `SIGTERM`
(each), and confirm `rc=0`, `session closed, tail durable`, and the last write is physically
in Cockroach after the process exits.

> **Result 5e: clean rc=0 on both signals; last write durable in Cockroach**

---

## 6. CLI over Cockroach

### 6a. Read verbs are lease-free readers

With **no** server running on the session, run each read verb against the cluster. They must
read directly without acquiring a lease.

```bash
lambo recall  --config lambo.cockroach.toml --session live-mcp --query "auth" --top-k 5
lambo saints  --config lambo.cockroach.toml --session live-mcp
lambo inspect --config lambo.cockroach.toml --session live-mcp --focus "user schema" --depth 2
lambo stats   --config lambo.cockroach.toml --session live-mcp
```

> **Result 6a: all read verbs return; no lease row created by a read; saints ordering correct**

### 6b. Write verbs take the lease and fail closed when a server owns the session

Start a server on `live-cli`. While it holds the lease, run a CLI `derive` on the same session
— it must be refused, naming the holder. Then stop the server and run the same `derive` — it
must acquire, write, and release.

```bash
# server holds it:
lambo serve --config lambo.cockroach.toml --session live-cli --agent srv --transport stdio &
lambo derive --config lambo.cockroach.toml --session live-cli --agent cli --content "cli write attempt" --kind Entity
# expect: refused, names 'srv'
# stop server, retry:
lambo derive --config lambo.cockroach.toml --session live-cli --agent cli --content "cli write ok" --kind Entity
# expect: acquired, wrote, released
```

> **Result 6b: refused while server owns; succeeds when free; lease released after**

### 6c. CLI ↔ MCP differential on the same data

Derive a concept over MCP, then read it back with `lambo recall` / `lambo saints` from the
CLI. The CLI output must agree with what the MCP `lambo_recall` returns for the same query.

> **Result 6c: CLI and MCP agree on the same session state**

---

## 7. Real embeddings + LFM2.5-230M + Pi agent (full stack)

This is the model-driven leg, end to end: a Pi agent running LFM2.5-230M drives the Lambo MCP
server, which is backed by Cockroach and real BGE-M3 embeddings.

### 7a. Two llama-server instances

BGE-M3 is Lambo's embedder; LFM2.5-230M is the Pi driver. Run them on different ports.

```bash
# BGE-M3 embedder for Lambo (port 8080; matches LAMBO_LLAMA_EMBED_URL)
hf download <bge-m3-gguf-repo> <bge-m3.gguf>
llama-server -m path/to/bge-m3.gguf --embedding --port 8080

# LFM2.5-230M driver for Pi (port 8081)
hf download LiquidAI/LFM2.5-230M-GGUF LFM2.5-230M-Q8_0.gguf
llama-server -m path/to/LFM2.5-230M-Q8_0.gguf --jinja -c 32768 --port 8081 -a lfm2.5-230m
```

Register `lfm2.5-230m` at `http://127.0.0.1:8081/v1` as a `local` provider in Pi
(`~/.pi/agent/models.json`), contextWindow 32768.

### 7b. Point Pi at the Cockroach-backed Lambo server

Create a project `.mcp.json`. Use `toolPrefix: none` and a `-t` allowlist so the small model
sees only Lambo's tools (a 230M model is easily hijacked by other extensions' tools).

```json .mcp.json
{
  "settings": { "toolPrefix": "none" },
  "mcpServers": {
    "lambo": {
      "command": "/path/to/lambo",
      "args": ["--config", "/path/to/lambo.cockroach.toml",
               "serve", "--session", "pi-live", "--agent", "pi-agent", "--transport", "stdio"],
      "lifecycle": "eager",
      "directTools": true
    }
  }
}
```

### 7c. Drive tools with the model

```bash
pi --provider local --model lfm2.5-230m --no-session --mode json \
  -t lambo_derive,lambo_recall,lambo_record_action,lambo_reserve,lambo_inspect,lambo_stats,lambo_saints,mcp \
  -p 'Call lambo_stats with {"agent_id":"pi-agent"}, then reply DONE.'
```

Then a write-then-read sequence to prove real embeddings + durable storage end to end:

```bash
pi --provider local --model lfm2.5-230m --no-session --mode json -t lambo_derive,lambo_recall,mcp \
  -p 'Call lambo_derive with agent_id "pi-agent" to store the concept content "The billing service retries failed charges" kind Entity. After it returns, call lambo_recall with agent_id "pi-agent" query "billing retries" top_k 3, then reply DONE.'
```

Confirm: the model issued a real tool call, `isError:false`, the concept is physically in
Cockroach, and `lambo_recall` returned it (which exercises the BGE-M3 vector leg, not a
fixture).

```bash
cockroach sql --url "$LAMBO_COCKROACH_DSN" -e "SELECT content FROM concepts WHERE session_id='pi-live';"
```

> **Result 7: model-driven derive+recall works end to end; concept durable in Cockroach; recall used real BGE embeddings; malformed model attempts are refused with honest errors**

### 7d. Swarm smell test (optional)

Run several concurrent `lambo derive` CLI calls (each a lease-held short write, or all through
the one `serve` writer) to stand in for a swarm, and watch `lambo_stats` for flush lag and
`degraded`. Confirm canonization collapses duplicate observations over time.

> **Result 7d (optional): concurrent writers behave; flush lag drains; duplicates canonize**

---

## 8. Verdict template

Copy this into a dated review record when the run completes.

```text
Live-Cockroach review — T8.2 + T8.3 — <date>, <cluster>, <commit>
Gates:            fmt [ ] clippy [ ] default-suite [ ] live --ignored [ ] vector-proof [ ]
Lease (4):        cross-process refuse [ ] release-on-close [ ] crash-expiry [ ] fencing-window [ ]
MCP (5):          durable [ ] N2-control-chars [ ] N1-cap+starvation [ ] N3/N4-leak [ ] shutdown-durable [ ]
CLI (6):          readers-lease-free [ ] write-lease-fail-closed [ ] CLI<->MCP-differential [ ]
Full stack (7):   model-driven derive+recall [ ] durable-in-cockroach [ ] real-BGE-recall [ ]
New findings:     <id / severity / file:line / repro / CONFIRMED|PLAUSIBLE>
Verdict:          CLEAN | REQUEST CHANGES
Notes:            <what could not be reproduced and why>
```

Record any new finding with a concrete repro and a severity, the same way the existing
`adve-review-t8.2-mcp.md` and `adve-review-t8.3-cli.md` do, and drop the filled verdict at the
top of this file so the next reader sees the outcome first.
