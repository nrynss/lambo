# lambo-cloudops — tool reference (control arm)

You are an agent working in a multi-agent cloud-ops environment. Lambo is
the memory layer for this environment. You are working against one Lambo
session.

---

## 0. Session and surfaces

- Every command targets one session. Use the session id from the environment
  (`LAMBO_SESSION`) or pass `--session <SESSION>` explicitly.
- MCP tools available to you: `lambo_recall`, `lambo_derive`,
  `lambo_record_action`, `lambo_inspect`. Every MCP tool takes your
  `agent_id`.
- `lambo_derive` and `lambo_record_action` are writers; they hold the
  session's single-writer lease. Sequence your writes; do not run two writer
  processes against one session. `lambo_recall` and `lambo_inspect` are
  read-only and may run freely.
- Never send a timestamp on any call. The server stamps interactions and
  edges itself.

---

## 1. The four tools

`lambo_recall` — Recall relevant memory for a query and return the Lambo
context block (whatever the server has on that query, rendered as text).
Arguments: `agent_id` (string, required — your agent id), `query` (string,
the natural-language thing you're asking about), plus optional parameters
for the token budget of the rendered block and how many graph hops to
expand.

`lambo_derive` — Derive one or more concepts from the current interaction
into session memory. Arguments: `agent_id` (string, required), `concepts`
(a list of concept objects — each has content and a kind, e.g. "resource"),
and optionally `parent_of` (a list of parent/child pairs to record a
hierarchy between concepts). Timestamps are stamped server-side; do not
send one.

`lambo_record_action` — Record an action you took, with what it produces,
modifies and depends on. Arguments: `agent_id` (string, required), `action`
(string, the action taken — e.g. "provisioned the auth middleware"), and
optionally `produces` (resources this action creates), `modifies`
(resources this action mutates), and `depends_on` (resources this action
requires) — each a list of resource-name strings. Timestamps are
stamped server-side; do not send one.

`lambo_inspect` — Inspect the neighbourhood around a concept: its type and
the edges out to a given depth. Arguments: `agent_id` (string, required),
`focus` (string, the concept content or node UUID to centre the view on),
and optionally `depth` (how many hops out from the focus to expand;
default 2, max 5).
