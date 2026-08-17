# Real-model swarm (C5) — LFM2-350M against lambo via MCP

Run `20260817-205024`, session `c-swarm-20260818`, **3 agents × 150 s**, on the
**Linux box** (cachyos-x8664, Ryzen 5 3600, 12 threads, CachyOS — not the MBP;
see the hardware caveat in `dev-diary/notes/concurrency-capture.md`).

## What was specified, and what happened

The specified swarm — OMP (Oh My Pi v17.3.5) as harness, `llama-server`
serving **LFM2-350M** (`LFM2-350M-UD-Q8_K_XL.gguf`, second instance on
`:8081`, OpenAI-compatible, `--jinja -c 32768 -a lfm2-350m`), N≥3 OMP agents
MCP-driving `lambo serve --transport http` on `:7701` (auth token, scratch
SQLite store, BGE-M3 embedder) against one scratch session — **is not
feasible with this model, and that failure is the finding:**

* **OMP model switching was configured and tried.** The local endpoint was
  registered as a provider in `~/.omp/agent/models.yml`
  (`lambolocal` → `http://127.0.0.1:8081/v1`, `api: openai-completions`,
  model id `lfm2-350m`) and invoked with `omp --model lfm2-350m -p`.
  The harness loaded the MCP server (`.mcp.json` → `:7701`, `Bearer` header,
  `lifecycle: eager` — the config is gitignored, carries the scratch token).
  **LFM2-350M cannot emit tool calls**: its reply to the derive instruction
  was garbled pseudo-tool text — prose fabricating a `lambo_derive` CLI
  (`lambo_derive schema auth_guards.json`, `lambo_derive audit_schema.yaml`,
  …) instead of the tool-call JSON OMP needs; the probe server logged zero
  `tools/call` requests and the probe store read back 0 interactions.
  Transcript committed under `probes/omp-harness-garbled-tool-call.txt`.
* **The raw OpenAI tools API was probed next** (`/v1/chat/completions` with a
  `tools` array for `lambo_derive`): the model returned prose with
  `finish_reason=stop` and **no `tool_calls`** — response committed under
  `probes/raw-tools-probe.json`. Confirmed at the protocol level: a 350M
  model cannot drive a tool-calling loop.

So the spec's fallback applied: **a minimal LLM loop** of
llama.cpp `/v1/chat/completions` + the streamable-HTTP MCP client pattern
(`scripts/loadtest/mcp_swarm.py`). Each agent thread prompts the real
LFM2-350M with a swarm topic and the previous recall, the model replies with
one JSON `{"concepts": [...]}` object, the loop calls `lambo_derive` and
`lambo_recall` over MCP, and every response lands in a JSONL ledger. The
model supplies the content; the loop supplies the tool-calling — the honest
description of what this model can do. All 487 concepts in the session are
LFM2-350M prose, delivered over the real MCP wire.

## Metrics (from the ledger, `ledger-20260817-205024.jsonl`)

| Metric | Value |
|---|---|
| Swarm agents | 3 concurrent (threads `swarm-0..2`) |
| Window | 149 s of derives |
| Derive calls (tasks) | 164 — **3961 derive-calls/hour** (7922 MCP calls/hour with recalls) |
| Recall calls | 164 |
| Model errors | 0 — no `model_error` ledger records (exceptions only); see footnote † |
| Concepts created / matched | 487 / 109 |
| **Canonization dedup rate** | **0.183** (109 matched existing of 596 concept references) — the swarm re-derived overlapping concepts onto existing canonical keys; the rate is low because LFM2-350M's concept texts drift between turns (verbose prose), so most derives create fresh nodes |

† **Model errors** counts HTTP/transport failures on the model call (0). It
does **not** count unparseable model replies: 54 of 218 model turns (25%)
returned text that parsed to 0 concepts and were recorded as `model_reply`
records with `parsed_concepts: 0` (they simply produced no derive that
turn) — material to reading "the model supplies the content".

Store readback after clean SIGTERM (`store-readback-20260817-205024.txt`):
interactions 164, concepts 487, edges 1522, **lease rows 0** — the ledger's
164 derives map 1:1 to store interactions and created == store concepts
exactly, and the server exited with the exact line
`lambo serve: session closed, tail durable` — `shutdown signal received` at
20:50:24.561330Z → `session closed, tail durable` at 20:50:24.562066Z, a
~0.7 ms transcript gap — releasing the single-writer lease.

## Artifacts

| Artifact | What it shows |
|---|---|
| `ledger-20260817-205024.jsonl` | Every swarm turn: model replies, parsed concepts, `lambo_derive` / `lambo_recall` results with the server's own created/matched counts |
| `stderr-serve-20260817-205024.log` | The lambo server's stderr: `session attached`, hybrid-degradation warning (SQLite has no VECTOR_SEARCH), the exact shutdown line |
| `store-readback-20260817-205024.txt` | Post-exit SQLite counts — ledger-exact, lease released |
| `portal-auth-middleware-*.png`, `portal-billing-retries-*.png` | **The visual**: `lambo serve-web` on the swarm session, recall queries answered by real LFM2-350M-derived concepts — first cards read "auth middleware guards schema integrity by validating the user schema before data access" and "billing service retries failed charges" (both strings verbatim in the swarm ledger's derive concepts), rendered as cards with score tracks (3200×1800, captured by `scripts/recording/capture-swarm-portal.mjs`, which waits for the query-specific content to render and fails on browser errors). The per-card numeric scores shown at capture time were read from the live DOM and appear in no committed artifact, so they are not quoted here. |
| `probes/` | Committed transcripts of the two tool-calling probes: `omp-harness-garbled-tool-call.txt` (the OMP-harness reply, prose + fabricated `lambo_derive` CLI, zero tool calls) and `raw-tools-probe.json` (raw `/v1/chat/completions` with a `tools` array: prose, `finish_reason=stop`, no `tool_calls`) — see `probes/README.md` |

## What was tried for OMP model configuration (the record)

1. Added the `lambolocal` provider (baseUrl `http://127.0.0.1:8081/v1`,
   `api: openai-completions`, `apiKey: LAMBO_SWARM_KEY`, model `lfm2-350m`)
   to `~/.omp/agent/models.yml` — the documented declarative registration
   ("the OpenAI-compatible endpoint anywhere").
2. `omp --model lfm2-350m -p --no-pty` in the workspace whose `.mcp.json`
   points at lambo `:7701`: harness booted, MCP tools discovered, model
   garbled the tool call (transcript committed under `probes/`).
3. Raw `POST /v1/chat/completions` with a `tools` array: no `tool_calls`
   emitted (response committed under `probes/raw-tools-probe.json`).

Steps 1–2 are OMP-config-feasible (the switch itself works; the model
cannot). This is the cut per P9's order: the swarm benchmark is optional,
and the failure is recorded rather than papered over.

## Reproduce

```bash
# llama-server already serving BGE-M3 on :8080; start the chat model:
llama-server -m <LFM2-350M-UD-Q8_K_XL.gguf> --port 8081 --jinja -c 32768 -a lfm2-350m
# lambo serve on :7701 (sqlite + bge_m3), then:
python3 scripts/loadtest/mcp_swarm.py --session c-swarm-<date> \
    --ledger evidence/swarm/ledger-<run>.jsonl --agents 3 --duration 150
PORTAL=http://127.0.0.1:7799 node scripts/recording/capture-swarm-portal.mjs
```
