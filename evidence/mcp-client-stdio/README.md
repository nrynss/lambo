# MCP server evidence — stdio and HTTP (2026-08-14)

**Superseded in part by J1 (`206f977`).** Per-call `agent_id` is now honoured, so
two things captured below no longer exist: the `attribution:` warning is deleted
outright (the caller's id *is* what gets recorded, so a warning saying otherwise
would be false), and the blanket cross-agent refusal is gone — a foreign id takes
and releases its own lock, and losing a race for a held node is a §11 conflict
naming the holder and the expiry. The "Lock survived" row still holds: a
non-holder still cannot release. Everything here is kept unedited as the
2026-08-14 capture, `.jsonl` transcripts included — they are wire captures, and a
capture is only evidence while it stays byte-exact.

Captured against `target/debug/lambo` built with
default features (`store-memory`, `embed-bge`, `embed-fixture`, `fixtures`).

No DSN or secret appears in this directory. The one config that names a
Cockroach DSN (fail-closed case B below) uses a redacted placeholder host.

## What was verified, and how

| # | Claim | Method | Result |
|---|---|---|---|
| 1 | A **real Claude Code client** completes the MCP handshake against `--transport stdio` | `claude mcp add` + `claude mcp list` / `claude mcp get` (Claude Code 2.1.226) | **PASS** — `✔ Connected` |
| 2 | Tool discovery lists all seven tools with usable schemas | `tools/list` over the real stdio transport | **PASS** — see `stdio-tools-list.jsonl` |
| 3 | `lambo_recall` returns the recall context block | `tools/call` over the real stdio transport | **PASS** — see below |
| 3b | **All seven tools** driven over the real stdio wire, requests *and* responses captured | `tools/call` × 7, during remediation | **PASS** — see `stdio-all-seven-tools.jsonl` |
| 4 | Streamable HTTP transport serves MCP | `curl POST /mcp` with `initialize` | **PASS** — 200, `mcp-session-id`, SSE result frame |
| 5 | Level B fails closed | four negative configs | **PASS** — see the table below |
| — | A **model-driven** tool call from Claude Code | `claude -p --allowedTools …` | **NOT VERIFIED** — see the honest limitation below |

### 1. Real Claude Code client handshake

```
$ claude mcp add lambo-t82 --scope local -- \
    /home/nryn/work/lambo/target/debug/lambo --config <SCRATCH>/lambo-mcp-test.toml \
    serve --session t8.2-evidence --agent agent-a --transport stdio
Added stdio MCP server lambo-t82 ... to local config

$ claude mcp list
Checking MCP server health…
lambo-t82: ... - ✔ Connected

$ claude mcp get lambo-t82
lambo-t82:
  Scope: Local config (private to you in this project)
  Status: ✔ Connected
  Type: stdio
```

Claude Code launched the binary as a subprocess, completed `initialize`, and
reported the server healthy. This is the definitive check that the chosen rmcp
rung interoperates with a live client, not only with unit tests. The registration
was removed afterwards (`claude mcp remove lambo-t82 -s local`) and was made in a
scratch directory, never in the repo.

### 2. Tool discovery — `stdio-tools-list.jsonl`

Seven tools, exactly the specified set:

```
lambo_derive, lambo_inspect, lambo_recall, lambo_record_action,
lambo_reserve, lambo_saints, lambo_stats
```

Each publishes a JSON-Schema object with a **required** `agent_id`, plus a
description. `initialize` also returns server instructions naming the session and
telling the model never to send a timestamp.

### 3. `lambo_recall` context block — `stdio-jsonrpc-session.jsonl`

A session over one stdio process driving **four** of the seven tools:
`initialize` → `lambo_derive` → `lambo_record_action` → `lambo_recall` →
`lambo_stats`.

> **Corrected during remediation.** This file, and the sentence that used to
> introduce it, claimed "all seven tools were driven end-to-end". It holds four
> — `lambo_reserve`, `lambo_inspect` and `lambo_saints` were never on the wire —
> and it records **responses only**, so the requests behind it cannot be
> checked. A later review caught the overclaim. The gap is closed by
> `stdio-all-seven-tools.jsonl` below rather than by rewording, and this file is
> kept as captured.

`lambo_recall(query = "update user schema")` returned this as the **text content
of the tool result, verbatim** — this is the recall renderer's output, not a summary:

```
user schema [Entity] (score 1.70)

created migrations/003.sql [Resource] (score 0.58)

migrations/003.sql [Resource] (score 0.52)

must stay backward compatible [Constraint] (score 0.51)

auth middleware [Entity] (score 0.46)
```

**Read this honestly:** the block carries no `canonical` marker, no `⚑ N nodes`
blast-radius line and no conflict line, because in a fresh session nothing has
been canonized (canonization needs `canonization_edge_min_age` to elapse), every
blast radius is below the warning threshold, and there is only one writer. Those
lines are rendered by the same `recall::format` path and are exercised by the
recall renderer's unit tests; producing them here needs the demo scenario's aged
session.
What this evidence proves is that the **real context block text reaches an MCP
client verbatim** — not that a canonical/conflict-annotated block was produced.

Structured content accompanies the text (hits with `node_id`, `score`,
`is_canonical`, `blast_radius`, plus warnings).

`lambo_stats` called with `agent_id: "agent-b"` returned the attribution warning
rather than silently rewriting the identity:

```
attribution: this process owns the session as agent 'agent-a'; the call from
'agent-b' is recorded in the graph as 'agent-a'. Per-call agent attribution needs
a Memory-level agent override (see T8.2 Handoff Log).
```

That is the message exactly as the binary emitted it on 2026-08-14, kept unedited
because it is a capture. The trailing pointer to an internal document was a leak of
project-internal vocabulary into a string returned to every agent, and has since been
removed from `src/mcp/server.rs`; the warning now ends "…until then, run one serve
process per agent." The same was done for the `lambo_reserve` refusal.

Server stderr for that run ends with:

```
INFO lambo::mcp::serve: mcp stdio: client disconnected reason=Closed
INFO lambo::mcp::serve: lambo serve: session closed, tail durable
```

— i.e. `Memory::close()` ran and the tail is durable.

### 3b. All seven tools on the wire — `stdio-all-seven-tools.jsonl`

Captured during remediation, one stdio process, session `t8.2-evidence`,
agent `agent-a`. **33 frames, requests and responses both**, each request
carrying a `note` saying what it demonstrates. Every tool call is a real
`tools/call` over the MCP wire protocol:

```
1/7 lambo_derive         isError=False  derived 3 concept(s): 3 created, 0 matched existing
2/7 lambo_record_action  isError=False  recorded action 'created migrations/003.sql': 2 concept(s), 2 edge(s)
3/7 lambo_recall         isError=False  user schema [Entity] (score 1.83) …
4/7 lambo_stats          isError=False  session 't8.2-evidence' (owner agent 'agent-a') nodes=7 edges=11 …
5/7 lambo_saints         isError=False  0 canonical memories in session 't8.2-evidence'
6/7 lambo_inspect        isError=False  focus: user schema [Entity] / hop 1: CoOccurrence -> auth middleware …
7/7 lambo_reserve        isError=False  reserved 478416c2-… until … for agent 'agent-a'
```

The same transcript carries the review fixes, each as a live wire experiment:

| Case | Frame note | Result on the wire |
|---|---|---|
| Foreign reserve | `agent-b` reserves a node `agent-a` holds | `isError=True` — *"refusing to take a soft lock on behalf of 'agent-b' … NOTHING WAS RESERVED OR RELEASED"* |
| Foreign release | `agent-c` releases `agent-a`'s lock | `isError=True` — same refusal |
| Lock survived | `lambo_inspect` after both refusals | `Reserved by agent-a until …` — the original lock survived |
| Attribution warning | `lambo_stats` as `agent-b` | the `attribution:` warning is now in the **text** content, after a `warnings:` line |
| Ambiguous focus | `lambo_inspect(focus="auth")` with three matches | `isError=True` — *"'auth' matches 3 concepts — name one exactly, or pass its node_id"*, candidates listed with node ids |
| Oversize argument | `lambo_record_action` with a 16 KiB + 1 action | `isError=True` — `action exceeds 16384 bytes (16385 given)` |
| Unknown field | `lambo_derive` with a client `created_at` | `isError=True` — `unknown field 'created_at', expected one of 'agent_id', 'concepts', 'parent_of'` |

Server stderr ended with `lambo serve: session closed, tail durable`, exit 0.

### 3c. Shutdown on a signal

An earlier review demonstrated that `Memory::close()` did **not** run on SIGINT or
SIGTERM under stdio, and that HTTP hung forever when a real client held its SSE
channel open. Re-running the same experiment against the fixed binary (spawned
with `preexec_fn=os.setsid`, so signal dispositions are the true defaults):

```
[stdio-SIGINT]              exited rc=0 after 0.00s   close() ran: True
[stdio-SIGTERM]             exited rc=0 after 0.00s   close() ran: True
[http-SIGINT  no SSE]       exited rc=0 after 0.00s   close() ran: True
[http-SIGINT  with SSE open]exited rc=0 after 5.02s   close() ran: True
[http-SIGTERM no SSE]       exited rc=0 after 0.00s   close() ran: True
[http-SIGTERM with SSE open]exited rc=0 after 5.02s   close() ran: True
```

That review measured `rc=-2` / `rc=-15` with `close() ran: False` for the two stdio
cases, and *never exited* for the two SSE cases. The 5.02 s is the bounded
grace window (`SHUTDOWN_GRACE`) expiring on the SSE stream that will never
finish on its own, after which the connection is dropped and the session
closes: the tail is flushed rather than held hostage.

### 4. HTTP transport

```
$ curl -s -i -X POST http://127.0.0.1:7731/mcp -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -d '{...initialize...}'
HTTP/1.1 200 OK
content-type: text/event-stream
mcp-session-id: 111440d5-6d6b-4e3d-82b5-9a6be4a91cb5

data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25", ...}}
```

SIGTERM shut it down through the graceful path; stderr again ended with
`session closed, tail durable`.

### 5. Level B — one resolve, fail closed

| Case | Config | Outcome | Exit |
|---|---|---|---|
| A | unknown TOML key `embedder.nonsense_key` | `unknown field 'nonsense_key', expected one of 'kind', 'dim', ...` | 1 |
| B | `store.kind = "cockroach"` (not compiled into default features) | `store kind 'cockroach' is not compiled into this binary; rebuild with --features store-cockroach` | 1 |
| C | `--transport grpc` | `unknown transport 'grpc' (expected 'stdio' or 'http')` | 2 |
| D | `--session` omitted | clap rejects the missing required flag | 2 |

In every case the process exits **before** a session is attached — no `Memory`,
no daemon, no flush task.

`serve` performs **one** `resolve_from_config_path`, in `main.rs`, and hands the
single `ResolvedBackends` into `Memory`. `mcp::serve::build_memory` deliberately
takes `ResolvedBackends` rather than a config path so a second resolve is not
expressible.

## Honest limitation — the model-driven call was not verified

`claude -p "<prompt that forces a lambo_* tool call>"` **failed in this
environment**, reproducibly, with:

```
"result": "Failed to authenticate: OAuth session expired and could not be refreshed"
"terminal_reason": "api_error"
```

This is an authentication failure in the nested Claude Code CLI — it never
reached the model, so no tool call was attempted. It is unrelated to the MCP
server: the same client, in the same directory, with the same registration,
health-checks the server as `✔ Connected` (evidence 1), which requires a
successful `initialize` handshake over stdio.

What is therefore **proven**: a real Claude Code client launches and handshakes
with `lambo serve`, and all seven tools — including `lambo_recall` returning the
context block — work over the real MCP wire protocol (evidence 3b; the handshake
itself is evidence 1, with a real client).

What is **not proven**: that a *model* chooses and invokes these tools, i.e. that
the descriptions and schemas are good enough for Claude to route to them
correctly. That needs a re-run with working credentials. Anyone with an
authenticated `claude` can reproduce it with the exact commands above.

## Reproducing

```bash
cargo build                     # default features are enough
cat > /tmp/lambo-mcp-test.toml <<'EOF'
[store]
kind = "memory"
[embedder]
kind = "fixture"
dim = 1024
EOF

claude mcp add lambo-t82 --scope local -- \
  "$PWD/target/debug/lambo" --config /tmp/lambo-mcp-test.toml \
  serve --session t8.2-evidence --agent agent-a --transport stdio
claude mcp list                 # expect: ✔ Connected
claude mcp remove lambo-t82 -s local
```
