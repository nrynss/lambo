# MCP tool reference

Lambo exposes seven tools over the Model Context Protocol. A running
`lambo serve --session <id>` process is the **single writer** for that session
(enforced by the T8.6 lease); MCP clients drive it as agents.

> Verified against `src/mcp/server.rs` at the T8.2/T8.6 merge. This documents the
> shipped binary, not the spec.

## Transports

| Transport | Flag | Notes |
|---|---|---|
| stdio | `--transport stdio` (default) | The universal path; works with any MCP client via the standard `mcpServers` config. |
| streamable HTTP | `--transport http [--port 7700] [--bind 127.0.0.1]` | Loopback by default. **Unauthenticated today** — auth / rate-limit / session caps are tracked in T8.7; do not expose a non-loopback bind until then. |

## Conventions (all tools)

- **Every tool takes `agent_id`** (string, required) — the agent identity making the call.
- **Never send a timestamp.** `created_at` and all clocks are stamped server-side (F18); there is no timestamp argument on any tool.
- **String limits:** every client-supplied string is capped at **16384 bytes** (`MAX_CONTENT_BYTES`) and must not contain control characters other than tab (`\t`) and newline (`\n`). Violations are refused with an honest message.
- **Ordering is the client's responsibility.** A read sees a write only after that write's own `tools/call` has returned. Sequence a `lambo_derive`/`lambo_record_action` before the `lambo_recall`/`lambo_inspect` meant to see it.
- **Errors** come back as tool results (`isError: true`) carrying a *class/summary*, not raw internal detail — URLs, DSNs, and driver text are redacted (N3/N4). Handler panics are contained and never cross the protocol.
- Unknown JSON fields are rejected (`deny_unknown_fields`).

## Tools

### `lambo_recall`
Recall relevant memory for a query and return the Lambo context block (canonical markers, blast-radius warnings, conflict lines).

| Arg | Type | Required | Bound / default |
|---|---|---|---|
| `agent_id` | string | yes | |
| `query` | string | yes | ≤16384 bytes |
| `top_k` | int | no | `1..=100`; default = config `default_top_k` (clamped to 100) |
| `max_tokens` | int | no | ≤100000 |
| `traversal_depth` | int | no | `0..=5`; default = config `default_traversal_depth` |

### `lambo_derive`
Derive concepts from the current interaction into session memory. Timestamps are stamped server-side; do not send one.

| Arg | Type | Required | Bound |
|---|---|---|---|
| `agent_id` | string | yes | |
| `concepts` | array of `{content, concept_type}` | yes | ≤64 entries (`MAX_CONCEPTS_PER_DERIVE`) |
| `concepts[].content` | string | yes | ≤16384 bytes |
| `concepts[].concept_type` | enum | yes | one of `Entity`, `Logic`, `Constraint`, `Resource`, `Observation` |
| `parent_of` | array of `{parent, child}` | no | hierarchy edges between the derived concepts |

### `lambo_record_action`
Record an action the agent took, with what it produces, modifies and depends on. Timestamps are stamped server-side.

| Arg | Type | Required | Bound |
|---|---|---|---|
| `agent_id` | string | yes | |
| `action` | string | yes | ≤16384 bytes |
| `produces` / `modifies` / `depends_on` | array of string | no | **combined total ≤64** (`MAX_ACTION_TARGETS`) |

### `lambo_reserve`
Take a soft lock on a memory node before editing it (or release one).

| Arg | Type | Required | Bound |
|---|---|---|---|
| `agent_id` | string | yes | **must be this server session's own agent** — a foreign `agent_id` is refused |
| `node_id` | string (UUID) | yes | |
| `ttl_seconds` | int | no | ≤3600 |
| `release` | bool | no | `true` releases instead of reserving |

Reservations are **advisory and RAM-local** — they do not survive a server restart. "No reservation" after a restart does not mean nobody else holds the node.

### `lambo_inspect`
Inspect the neighbourhood around a concept: its type, canonization status, blast radius and typed edges out to a depth.

| Arg | Type | Required | Bound |
|---|---|---|---|
| `agent_id` | string | yes | |
| `focus` | string | yes | ≤16384 bytes |
| `depth` | int | no | `0..=5`; neighbourhood capped at 200 nodes |

### `lambo_saints`
List the session's canonical memories — concepts that earned Canonical status through the audited transition path. Takes only `agent_id`.

### `lambo_stats`
Session health: flush lag, write-behind log depth, node/edge/concept counts, canonization progress and degraded state. Takes only `agent_id`.

## Server instructions (`get_info`)

The server advertises these instructions to the model:

> Lambo agentic graph memory for session '<id>'. Call lambo_recall before acting on a task to load relevant prior memory, lambo_derive and lambo_record_action to write what you learned and did, lambo_reserve before editing a shared concept, and lambo_inspect / lambo_saints / lambo_stats to look around. Every tool takes your agent_id. Never send a timestamp: the server stamps them. Ordering is yours to manage: a read sees a write only after that write's own tool call has returned.

## Caps at a glance

| Constant | Value |
|---|---|
| `MAX_CONTENT_BYTES` (every client string) | 16384 |
| `MAX_TOP_K` | 100 |
| `MAX_TRAVERSAL_DEPTH` / `MAX_INSPECT_DEPTH` | 5 |
| `MAX_MAX_TOKENS` | 100000 |
| `MAX_CONCEPTS_PER_DERIVE` | 64 |
| `MAX_ACTION_TARGETS` (produces+modifies+depends_on) | 64 |
| `MAX_RESERVE_TTL_SECS` | 3600 |
| `MAX_INSPECT_NODES` | 200 |

See also: [cli.md](cli.md) for the equivalent CLI verbs, [api.md](api.md) for the underlying `Memory` API.
