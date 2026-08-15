# MCP client interop — Cursor Agent CLI, and Claude Code against the managed server

Captured **2026-08-15**. Extends the stdio evidence in `../mcp-client-stdio/` with a
**third independent MCP client**, and captures a model-driven run against the managed
CockroachDB MCP server.

No DSN, key, or cluster id appears in this directory. The Cursor workspace was a scratch
directory outside the repo; nothing was written into the repo by either agent.

<a id="cursor-cockroach-auth"></a>
**Note on `cockroachdb-cloud: requires_authentication` in the Cursor transcripts.** Three
of the Cursor captures here show that line, and it was accurate when each was taken. It
went stale the same day: the connector was authorized and `cursor-agent mcp list` now
reports `cockroachdb-cloud: ready`. Those captures are left as recorded; the run that
followed is in `cursor-model-driven-managed-mcp.txt`.

## Results

| # | Claim | Method | Result |
|---|---|---|---|
| 1 | Cursor Agent CLI completes the MCP handshake against `--transport stdio` | `cursor-agent mcp list` → `lambo: ready` | **PASS** |
| 2 | Cursor discovers all seven tools with their argument names | `cursor-agent mcp list-tools lambo` | **PASS** — 7/7 |
| 3 | **A model drives Lambo's tools from Cursor**, end to end | `cursor-agent -p` running derive → record_action → recall → stats | **PASS** |
| 4 | Claude Code completes the handshake against the **managed CockroachDB MCP server** | `claude mcp list` → `✔ Connected` | **PASS** |
| 5 | **Claude Code model-driven calls against the managed server** | four `mcp__cockroachdb-cloud__*` calls in sequence | **PASS** |
| 6 | **Cursor model-driven calls against the managed server**, agreeing with 5 | interactive `cursor-agent`, `list_databases` + two `select_query` | **PASS** |

Files: `cursor-handshake-and-tools.txt` (1, 2), `cursor-model-driven-four-tools.txt` (3),
`claude-code-managed-mcp-connected.txt` (4), `claude-code-model-driven-managed-mcp.txt` (5),
`cursor-model-driven-managed-mcp.txt` (6). The approval re-check that items 3 and 6 depend
on is `cursor-approval-recheck.txt`.

## Why item 3 matters

The earlier stdio capture verified the handshake and tool discovery from Claude Code, but
recorded a model-driven tool call as **not verified**. That gap is closed here by a
different client. Cursor's model chose the arguments, issued four `tools/call` requests in
sequence against one stdio process, and got coherent state back: `lambo_recall` ranked
`user schema` at 1.12 above its two children (0.84, 0.50) and the action node it had just
recorded (0.49), and `lambo_stats` reported the accumulated graph (`nodes=6 edges=11
concepts=4`, `degraded=false`). Nothing was scripted at the JSON-RPC layer — the model
drove it.

Combined with the OMP leg, **two** independent clients have now driven Lambo's tools
model-first.

## Why item 5 matters

The `mcp__cockroachdb-cloud__*` tools were in the assistant session's own tool roster, so
the calls were made directly rather than through a subprocess. Four calls in sequence —
`list_clusters` → `list_databases` → `list_tables` → `select_query` — with the model
choosing the arguments.

The payoff is not that the query ran, it is what it returned. The published query on the
demo page, scoped to one session, returns node `724c92b9` walking
`None → Candidate → Venerable → Canonical` over 1.29s, with `blast_radius` null on the two
earlier hops and **9** on the promotion — exactly what the page claims of it. That 9 also
matches the recall block published on the same page ("blast radius 9"). The number the demo
narrates and the number durably written to CockroachDB are the same number, checked from
opposite ends.

So the managed server now has model-driven legs from **three** clients: OMP
(`../managed-mcp-canonization-events.md`), Claude Code, and Cursor
(`cursor-model-driven-managed-mcp.txt`). All three return the same five rows and agree on
every field. Different clients, different models, one durable record.

### What the Cursor run also settled about approvals

Cursor's run was interactive, and **nothing was auto-rejected — `--force` was neither
passed nor needed.** Against the same account and the same build, print mode rejects every
call (`cursor-approval-recheck.txt`). So the per-call approval gate belongs to
non-interactive print mode, not to the client, the account, or the server. Interactive
approval is the supported path; `--force` is the scripting workaround, and it is the only
path that needs bounding.

### Footnote: why the nested `claude -p` attempt failed

Worth recording so nobody re-runs it. Nested `claude -p` fails with
`Failed to authenticate: OAuth session expired and could not be refreshed`, and
**re-authenticating does not fix it**. It is not a stale token:

- It fails on `claude -p "say hi"` too, so it is unrelated to MCP or to the query.
- It still fails with `CLAUDE_CODE_CHILD_SESSION`, `ANTHROPIC_BASE_URL`, the messaging
  socket vars and friends all stripped, so it is not environment inheritance.
- `~/.claude/.credentials.json` holds an access token with **no refresh token** and a
  falsy `expiresAt`. A standalone CLI process has nothing to refresh with, so it reports
  the session as unrefreshable and exits.

That credential shape is what a host-app-managed session looks like; the host refreshes
in-process. Non-model commands like `claude mcp list` are unaffected, which is why item 4
passed while item 5 first appeared blocked. The fix was not to authenticate again — it was
to stop shelling out and call the tools in-session.

## Reproducing the Cursor run

```
cursor-agent mcp enable lambo
cursor-agent mcp list-tools lambo
cursor-agent -p --trust --approve-mcps --sandbox enabled --force "<prompt>"
```

Both approval steps are real and were **re-verified from a clean scratch workspace on
2026-08-15**, after the account was re-authenticated. Neither has gone away:

1. Without `cursor-agent mcp enable lambo`, `mcp list` reports
   `lambo: not loaded (needs approval)` and `list-tools` fails with
   `Failed to load MCP 'lambo': MCP server "lambo" has not been approved`.
2. `--approve-mcps` approves **loading** the server but **not** individual tool calls. In
   non-interactive print mode each call is auto-rejected. A re-run with `--approve-mcps`
   and no `--force` ended with the agent reporting:
   `lambo_derive was rejected twice (User rejected MCP: lambo-lambo_derive), so steps 2
   and 3 were not run.` Tool discovery still worked (7/7); only the calls were refused.

`--sandbox enabled` is deliberate: `--force` is required to get past per-call MCP approval
in print mode, and the sandbox bounds what else the agent can touch.
