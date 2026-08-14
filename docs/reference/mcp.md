# MCP tools

Lambo is agentic graph memory. It runs as an MCP server that exposes seven tools your agent can call to store what it learns, recall relevant prior memory, and coordinate edits. A running `lambo serve --session <id>` process is the single writer for that session, and your agents connect to it as MCP clients.

## Transports

Lambo serves over stdio by default, and over HTTP when you ask for it.

| Transport | Command | Notes |
|---|---|---|
| stdio | `lambo serve --session demo` | The default. Works with any MCP client through the standard `mcpServers` configuration. |
| HTTP | `lambo serve --session demo --transport http` | Streamable HTTP on port `7700`, bound to `127.0.0.1`. The HTTP endpoint has no authentication yet, so keep it on localhost. |

## What every tool expects

Every tool takes an `agent_id`, which is the identity of the agent making the call.

Do not send timestamps. Lambo stamps all times itself, and no tool accepts a timestamp argument.

Any string you send is limited to 16 KB, and it may not contain control characters other than tab and newline. Lambo rejects anything larger or malformed with a clear message.

You manage ordering. A read sees a write only after that write's tool call has returned. If you want a recall to see something you just derived, call `lambo_derive` first, wait for it to return, and then call `lambo_recall`.

Errors come back as normal tool results with `isError` set to `true` and a short reason. Lambo never includes internal detail such as endpoints or connection strings in an error.

## Tools

### lambo_recall

Recall relevant memory for a query. This returns the Lambo context block, which is the set of memories that matter for the query, with markers for canonical facts, blast-radius warnings, and conflicts.

| Argument | Type | Required | Range or default |
|---|---|---|---|
| `agent_id` | string | Yes | |
| `query` | string | Yes | Up to 16 KB. |
| `top_k` | integer | No | 1 to 100. Defaults to the configured value. |
| `max_tokens` | integer | No | Up to 100,000. |
| `traversal_depth` | integer | No | 0 to 5. Defaults to the configured value. |

### lambo_derive

Store new concepts learned from the current interaction.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `agent_id` | string | Yes | |
| `concepts` | array of `{content, concept_type}` | Yes | Up to 64 per call. |
| `concepts[].content` | string | Yes | Up to 16 KB. |
| `concepts[].concept_type` | enum | Yes | One of `Entity`, `Logic`, `Constraint`, `Resource`, or `Observation`. |
| `parent_of` | array of `{parent, child}` | No | Hierarchy links between the concepts you are deriving. |

### lambo_record_action

Record something the agent did, along with what it produces, modifies, and depends on.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `agent_id` | string | Yes | |
| `action` | string | Yes | Up to 16 KB. |
| `produces`, `modifies`, `depends_on` | array of string | No | Up to 64 entries combined across the three. |

### lambo_reserve

Take a soft lock on a memory node before you edit it, so two agents do not edit the same concept at once. Set `release` to `true` to release the lock instead.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `agent_id` | string | Yes | Must be the agent this server runs as. A different `agent_id` is refused. |
| `node_id` | string (UUID) | Yes | |
| `ttl_seconds` | integer | No | Up to 3600. |
| `release` | boolean | No | Release instead of reserve. |

Reservations are advisory and live in memory only, so they do not survive a server restart. After a restart, the absence of a reservation does not guarantee that nobody else is working on the node.

### lambo_inspect

Look at the neighbourhood around a concept, including its type, whether it is canonical, its blast radius, and the edges leading out from it.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `agent_id` | string | Yes | |
| `focus` | string | Yes | Up to 16 KB. |
| `depth` | integer | No | 0 to 5. The neighbourhood is limited to 200 nodes. |

### lambo_saints

List the session's canonical memories, which are the concepts that Lambo has promoted to canonical facts. This tool takes only `agent_id`.

### lambo_stats

Report session health, including how far behind durable storage the session is, write-log depth, node, edge, and concept counts, canonization progress, and whether the session is degraded. This tool takes only `agent_id`.

## How the server introduces itself

When your client connects, the server describes itself to the model:

> Call `lambo_recall` before acting on a task to load relevant prior memory. Call `lambo_derive` and `lambo_record_action` to write what you learned and did. Call `lambo_reserve` before editing a shared concept, and `lambo_inspect`, `lambo_saints`, or `lambo_stats` to look around. Every tool takes your agent_id. Never send a timestamp, because the server stamps them. Ordering is yours to manage: a read sees a write only after that write's tool call has returned.

## Limits

| Limit | Value |
|---|---|
| Any single string | 16 KB |
| `top_k` | 100 |
| `traversal_depth` and inspect depth | 5 |
| `max_tokens` | 100,000 |
| Concepts per `lambo_derive` | 64 |
| Targets per `lambo_record_action`, combined | 64 |
| Reservation TTL | 3600 seconds |
| Nodes returned by `lambo_inspect` | 200 |

See [Command line](cli.md) for the same operations from the terminal, and [Library API](api.md) for the underlying Rust library.
