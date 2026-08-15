# Evidence — the managed CockroachDB MCP server answered

Captured **2026-08-15**. Closes the `Managed MCP server` row that the claim audit
marked **unevidenced**.

## What this proves

The CockroachDB Cloud **managed MCP server** (`https://cockroachlabs.cloud/mcp`, http
transport, `mcp-cluster-id` header pointing at the `nrynss` demo cluster) served a live
query against the demo data, driven by a model through MCP tool calls. This is one of the
two CockroachDB tools the submission claims under spec §12.1. No `psql`, no direct DSN —
the query went over MCP and the managed server returned the rows.

## MCP client used: OMP

At capture time this was the only client that could reach the managed server. Claude Code's
`cockroachdb-cloud` registration was present in `~/.claude.json` and in the gitignored
`.mcp.json`, but the OAuth handshake to `cockroachlabs.cloud` had **not** been completed, so
Claude Code reported the server under "requires authentication" and exposed no cockroach
tools. Restarting a session does not fix that — registration is not a token.

**Superseded later the same day.** That OAuth was completed, and Claude Code then drove the
same managed server model-first; see
[`mcp-client-interop/claude-code-model-driven-managed-mcp.txt`](mcp-client-interop/claude-code-model-driven-managed-mcp.txt).
The two captures agree on the substance: the same `Candidate → Venerable → Canonical` walk
at blast radius 9, reached through two different MCP clients. The OMP capture below stands
as written.

The capture therefore ran through **OMP v17.3.4** (model `deepseek/deepseek-v4-flash:high`),
which already holds a valid connection to the same managed server. OMP required
`INFERX_API_KEY` from `~/.zshrc` to boot; the `pi-inferx-provider` plugin registers the
provider at runtime, which is why clearing `models.yml` did not stop the startup crash.

Tool calls were restricted to the read-only subset
(`select_query`, `list_clusters`, `list_databases`, `list_tables`, `get_table_schema`,
`get_cluster`) so nothing could write.

## Query

Scoped by `session_id` — the cluster also holds ~2833 seeded concepts and 240 events across
many sessions. **Always scope.** The database is `lambo`; an unqualified run hits `defaultdb`
and fails with `relation "canonization_events" does not exist`.

```sql
SELECT node_id, from_status, to_status, blast_radius, occurred_at
FROM canonization_events
WHERE session_id = 'demo-rest-api-bdd69691-ea92-41b7-ad3a-7506332071dc'
ORDER BY occurred_at;
```

## Result — tool that answered: `mcp__cockroachdb_cloud_select_query`

| node_id | from_status | to_status | blast_radius | occurred_at |
|---|---|---|---|---|
| 724c92b9-a8a0-4d58-aa2e-29ed3cc43a57 | None | Candidate | null | 2026-08-15T14:50:08.306429Z |
| 3e3a6984-3bd6-480d-8490-d9df9b03540a | None | Candidate | null | 2026-08-15T14:50:08.306429Z |
| 724c92b9-a8a0-4d58-aa2e-29ed3cc43a57 | Candidate | Venerable | null | 2026-08-15T14:50:08.992263Z |
| 724c92b9-a8a0-4d58-aa2e-29ed3cc43a57 | Venerable | Canonical | 9 | 2026-08-15T14:50:09.600577Z |
| 1848c6e5-8b63-4a64-942e-76c75afd0ce1 | None | Candidate | null | 2026-08-15T14:50:10.103585Z |

Row count: **5**.

## Node labels (same MCP tool, `concepts.content`)

```sql
SELECT id, content FROM concepts
WHERE id IN ('724c92b9-a8a0-4d58-aa2e-29ed3cc43a57',
             '3e3a6984-3bd6-480d-8490-d9df9b03540a',
             '1848c6e5-8b63-4a64-942e-76c75afd0ce1')
```

| id | content |
|---|---|
| 724c92b9-… | `user schema` |
| 3e3a6984-… | `add oauth_id to user schema` |
| 1848c6e5-… | `add rate limiting middleware` |

## Cross-check against the demo's golden numbers

`user schema` is the node that walks **Candidate → Venerable → Canonical**, and the
Canonical transition carries **blast_radius 9** — matching the golden blast radius of 9
recorded for the demo in the claim audit and in `evidence/demo-live-1.txt`.
`blast_radius` is null on the non-terminal hops, which is the documented shape (only the
promotion gate measures it).

## For the demo video

This is the split-screen beat the script calls for: the agent transcript on one side, this
query returning the five-row status walk from the managed console-side MCP server on the
other. Re-run with the command in the "MCP client used" section above; it takes one call.
