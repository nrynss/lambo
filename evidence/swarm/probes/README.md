# C5 — LFM2-350M tool-calling probes (committed outputs)

The C5 finding rests on two probes showing the 350M chat model cannot emit MCP
tool-call JSON. The original probe runs (2026-08-17 ~20:45 UTC) were
observations in the implementer's session, not committed artifacts; both were
**re-run on 2026-08-17 21:05 UTC** against the same model server and the same
OMP provider configuration, and the outputs below are the re-run transcripts
(they reproduce the original finding — see the swarm runbook). Nothing in
either output is redacted; the scratch auth token never appears in them.

## Probe 1 — under the OMP harness (`omp-harness-garbled-tool-call.txt`)

Setup, identical to the swarm runbook's record:

* `~/.omp/agent/models.yml` provider `lambolocal` → `http://127.0.0.1:8081/v1`
  (`api: openai-completions`, `apiKey: LAMBO_SWARM_KEY` env var, model
  `lfm2-350m`) — OMP 17.3.5.
* A workspace whose `.mcp.json` registers lambo over streamable HTTP
  (`http://127.0.0.1:7701/mcp`, `Authorization: Bearer <scratch token>`,
  `lifecycle: eager`), pointing at a scratch `lambo serve` (SQLite store,
  BGE-M3 embedder).
* Command: `omp --model lfm2-350m -p --no-pty "Use the lambo_derive tool to
  derive the concept 'auth middleware guards schema integrity' into the memory
  graph."`

Result (captured verbatim in the file): the harness returned the model's
assistant text as the answer — a long prose reply that *fabricates* a
`lambo_derive` CLI (`lambo_derive schema auth_guards.json`,
`lambo_derive audit_schema.yaml`, …) instead of emitting the
`tools/call`-shaped tool-call JSON the harness needs. The probe server's
stderr shows the session attach and **zero** `tools/call` requests, and the
probe store readback shows **0 interactions / 0 concepts** — the model never
drove a tool call.

## Probe 2 — raw OpenAI tools API (`raw-tools-probe.json`)

`POST /v1/chat/completions` to `http://127.0.0.1:8081/v1` with a `tools`
array for `lambo_derive` (same model, `tool_choice: auto`), the request the
README's "What was tried" step 3 describes. The captured response (verbatim,
JSON) shows `finish_reason: "stop"`, a prose reply, and **no `tool_calls`
key** in the message — the protocol-level confirmation that the model does
not emit tool calls even when the tools API hands it the schema.

## What these probes establish

The model's own output is the evidence: under the harness it produces
pseudo-tool prose, and at the raw protocol level it returns `finish_reason:
stop` with no `tool_calls`. That is why the spec's fallback (a minimal LLM
loop, `scripts/loadtest/mcp_swarm.py`, where the *loop* supplies the
tool-calling) is the honest description of what this model can do.
